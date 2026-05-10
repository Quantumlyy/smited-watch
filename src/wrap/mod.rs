//! Process wrapping orchestrator: spawn child, tee its output, scan,
//! fire triggers, propagate exit code.
//!
//! This is the heart of smited-watch. It glues together:
//!
//! * [`spawn`]: PTY-vs-pipes child spawn and stdio handles
//! * [`tee`]: blocking reader threads + bounded mpsc + async tee tasks
//! * [`stdin`]: parent stdin → child PTY (raw mode + Drop guard)
//! * [`signal`]: SIGINT/SIGTERM forwarding, SIGWINCH PTY resize
//! * [`crate::scan`]: line-buffered RegexSet matching
//! * [`crate::debounce`]: per-pattern leading-edge debouncing
//! * [`crate::trigger`]: building TriggerRequest messages
//! * [`crate::client`]: fire-and-forget gRPC dispatch
//! * [`crate::exit`]: exit-code → success/failure sensation with dedupe
//!
//! ## Channel topology
//!
//! ```text
//! [pty/pipe reader thread]──Bytes──▶ mpsc(1024) ──▶ [tee task]
//!                                                    ├─▶ stdout/stderr.write_all
//!                                                    └─▶ mpsc(1024) ──▶ [scan task] ──▶ Scanner.feed → trigger.fire
//! ```
//!
//! In pipe mode there are two independent reader→tee chains (one for
//! stdout, one for stderr) feeding a single shared scan channel. In PTY
//! mode there is one reader→tee chain (the master fd carries both child
//! streams merged).

use std::ffi::OsString;
use std::process::ExitStatus;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, warn};

use crate::client::TriggerClient;
use crate::config::{OnExit, Pattern};
use crate::debounce::{Debouncer, Decision};
use crate::scan::{Scanner, StreamId};
use crate::trigger::build_pattern_trigger;

pub mod signal;
pub mod spawn;
pub mod stdin;
pub mod tee;

use self::signal::{install_handlers, MasterPtyHandle, ShutdownState, SignalTarget};
use self::spawn::{spawn as spawn_child, ChildIo};
use self::stdin::{spawn_stdin_forwarder, RawModeGuard};
use self::tee::{spawn_blocking_reader, tee_task, Drops, SinkKind, CHANNEL_CAPACITY};

/// All the inputs the orchestrator needs to wrap a command.
pub struct WrapOptions {
    pub cmd: Vec<OsString>,
    pub patterns: Arc<Vec<Pattern>>,
    pub debouncers: Arc<Vec<Debouncer>>,
    pub trigger_client: Arc<TriggerClient>,
    pub default_backend_id: String,
    pub on_exit: OnExit,
    /// When true, skip PTY allocation entirely (forces pipes mode). Used
    /// in tests and `SMITED_WATCH_DISABLE` mode where TTY semantics aren't
    /// needed.
    pub force_pipes: bool,
}

/// Wrap the command end-to-end. Returns when the child has exited and the
/// in-flight trigger calls have had ~1s to drain.
///
/// The exit sensation (success or failure) is fired here, after pattern
/// dedupe logic in [`crate::exit::handle`].
pub async fn run(opts: WrapOptions) -> Result<ExitStatus> {
    let started = Instant::now();
    let use_pty = !opts.force_pipes && spawn::parent_is_tty();
    let child_io = spawn_child(&opts.cmd, use_pty)?;

    // ──────────────────────────────────────────────────────────────────
    // Wire up shared state
    // ──────────────────────────────────────────────────────────────────
    let scanner = Arc::new(Scanner::new(opts.patterns.clone())?);
    let last_pattern_fire: Arc<StdMutex<Option<Instant>>> = Arc::new(StdMutex::new(None));
    let scan_drops: Drops = Arc::new(AtomicU64::new(0));
    let shutdown_state = Arc::new(ShutdownState::default());
    // Notified by tee tasks when their parent-stdout/stderr write fails
    // with `BrokenPipe` — i.e. the downstream pipeline consumer
    // (`| head -n1`, etc.) has exited. The orchestrator's pipe-broken
    // watcher then forwards SIGPIPE to the child so the child dies
    // promptly instead of writing into the void forever.
    let pipe_broken: Arc<Notify> = Arc::new(Notify::new());

    // Single shared scan channel, fed by all tee tasks (1 in PTY mode, 2
    // in pipe mode). Each chunk carries its `StreamId` so the scanner
    // can route bytes into the right per-stream line buffer — without
    // this tagging, a partial stdout line and a stderr chunk would
    // splice together and produce phantom matches.
    let (scan_tx, scan_rx) = mpsc::channel::<(StreamId, Bytes)>(CHANNEL_CAPACITY);

    // Spawn the scanner consumer task before any tee task so we never
    // block tee on a not-yet-ready scanner.
    let scan_consumer = tokio::spawn(scan_consumer_task(
        scan_rx,
        scanner.clone(),
        opts.patterns.clone(),
        opts.debouncers.clone(),
        opts.trigger_client.clone(),
        opts.default_backend_id.clone(),
        last_pattern_fire.clone(),
    ));

    // ──────────────────────────────────────────────────────────────────
    // Spawn per-stream tee chains
    // ──────────────────────────────────────────────────────────────────
    let mut reader_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut tee_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut raw_mode_guard: Option<RawModeGuard> = None;
    let mut stdin_thread: Option<std::thread::JoinHandle<()>> = None;

    let exit_status: ExitStatus;
    let signal_aborts: Vec<tokio::task::AbortHandle>;
    match child_io {
        ChildIo::Pty(mut pty) => {
            // PTY mode: stdin raw-mode + forwarder + single output tee.
            raw_mode_guard = Some(RawModeGuard::enable());
            stdin_thread = Some(spawn_stdin_forwarder(pty.writer));

            let (out_tx, out_rx) = mpsc::channel::<Bytes>(CHANNEL_CAPACITY);
            reader_threads.push(spawn_blocking_reader(pty.reader, out_tx));
            tee_tasks.push(tokio::spawn(tee_task(
                out_rx,
                SinkKind::Stdout,
                scan_tx.clone(),
                scan_drops.clone(),
                pipe_broken.clone(),
            )));

            // PTY-mode children are session leaders via portable-pty's
            // setsid, so always Pgrp targeting.
            let target = SignalTarget::new(pty.child.process_id(), true);
            let master_arc: Arc<MasterPtyHandle> = Arc::new(MasterPtyHandle::new(pty.master));
            signal_aborts = install_handlers(target, Some(master_arc), shutdown_state.clone())?;
            let pipe_watcher = spawn_pipe_broken_watcher(pipe_broken.clone(), target);

            // Wait for the child on a blocking thread (portable-pty's
            // `Child` is sync). The Child handle is owned by this
            // closure; the signal handler doesn't need it because it
            // forwards via the saved PID directly.
            let exit = tokio::task::spawn_blocking(move || -> Result<ExitStatus> {
                let status = pty.child.wait()?;
                Ok(portable_status_to_exit_status(&status))
            })
            .await??;
            exit_status = exit;
            pipe_watcher.abort();
        }
        ChildIo::Pipes(mut pipes) => {
            // Pipes mode: child stdout & stderr are already tokio AsyncRead
            // — no blocking-reader thread needed; we tee them directly.
            let scan_tx_a = scan_tx.clone();
            let drops_a = scan_drops.clone();
            let stdout = pipes.stdout;
            let pb_a = pipe_broken.clone();
            tee_tasks.push(tokio::spawn(async move {
                async_tee(stdout, SinkKind::Stdout, scan_tx_a, drops_a, pb_a).await;
            }));
            let scan_tx_b = scan_tx.clone();
            let drops_b = scan_drops.clone();
            let stderr = pipes.stderr;
            let pb_b = pipe_broken.clone();
            tee_tasks.push(tokio::spawn(async move {
                async_tee(stderr, SinkKind::Stderr, scan_tx_b, drops_b, pb_b).await;
            }));

            // Pipe-mode children are pgrp leaders only when stdin was
            // non-TTY at spawn time (see spawn_pipes for the rationale).
            let target = SignalTarget::new(pipes.child.id(), pipes.pgrp_leader);
            signal_aborts = install_handlers(target, None, shutdown_state.clone())?;
            let pipe_watcher = spawn_pipe_broken_watcher(pipe_broken.clone(), target);

            // The child handle stays in this scope; we don't share it
            // with the signal handler, which forwards via the saved PID.
            exit_status = pipes.child.wait().await?;
            pipe_watcher.abort();
        }
    }

    // ──────────────────────────────────────────────────────────────────
    // Drain
    // ──────────────────────────────────────────────────────────────────
    // Closing scan_tx here unblocks the scanner consumer once all tee
    // tasks have finished forwarding their final bytes.
    drop(scan_tx);

    // Wait for tee tasks (they exit when their reader-mpsc closes).
    for t in tee_tasks {
        let _ = t.await;
    }
    // Reap blocking reader threads (they exit on EOF or send-fail).
    for t in reader_threads {
        let _ = t.join();
    }
    // Stdin thread will keep going until parent stdin EOFs or write fails.
    // We don't join it — the process will exit shortly.
    drop(stdin_thread);

    // Wait for the scan consumer to drain every queued chunk *first*. Only
    // after `scan_consumer.await` returns is the scanner's internal buffer
    // guaranteed to contain all bytes produced by the child — calling
    // `scanner.flush()` earlier would emit a stale partial line and we'd
    // never see the trailing one for commands that print a final line
    // without a `\n`.
    let _ = scan_consumer.await;

    // Now flush trailing partial lines for every stream and dispatch.
    let final_events = scanner.flush_all();
    if !final_events.is_empty() {
        dispatch_events(
            &final_events,
            &opts.patterns,
            &opts.debouncers,
            &opts.trigger_client,
            &opts.default_backend_id,
            &last_pattern_fire,
        );
    }

    let drops = scan_drops.load(std::sync::atomic::Ordering::Relaxed);
    if drops > 0 {
        debug!(dropped_chunks = drops, "scanner dropped chunks during run");
    }

    // Fire on-exit sensation (with dedupe), unless we're shutting down due
    // to a signal.
    let last_fire = *last_pattern_fire.lock().expect("last-fire mutex poisoned");
    crate::exit::handle(crate::exit::ExitContext {
        status: exit_status,
        duration: started.elapsed(),
        on_exit: &opts.on_exit,
        default_backend_id: &opts.default_backend_id,
        last_pattern_fire: last_fire,
        trigger_client: &opts.trigger_client,
        shutdown_due_to_signal: shutdown_state.was_signal(),
        now: Instant::now(),
    });

    // Stop the signal handlers so they don't keep listening past the
    // wrapped command's lifetime. (They'd be cancelled when main drops
    // the runtime anyway, but releasing them now is cleaner.)
    for h in signal_aborts {
        h.abort();
    }

    // Spec: "Wait up to 1s for any in-flight trigger calls to complete.
    // Log warnings for stragglers but exit anyway." `drain` actually
    // awaits the spawned tasks rather than sleeping a fixed duration, so
    // we exit promptly when fires complete and only pay the full 1s if
    // something is genuinely stuck.
    opts.trigger_client.drain(Duration::from_secs(1)).await;

    // Restore terminal cooked mode before returning.
    drop(raw_mode_guard);

    Ok(exit_status)
}

/// Watch for the "downstream pipeline consumer closed our output" signal
/// from a tee task and, when it fires, forward SIGPIPE to the child so
/// it dies promptly rather than getting EPIPE on its next write attempt
/// (which for a quiet child could be much later, leaving the wrapper
/// blocked on `child.wait()` forever).
///
/// The handle returned by this function should be aborted once the child
/// has exited so the watcher doesn't leak past the wrapper's lifetime.
/// On platforms without `forward_signal` support (Windows in v0.1) the
/// watcher logs the broken-pipe event and the wrapper still relies on
/// the OS pipe-buffer fill + child's own handling to wind things down.
fn spawn_pipe_broken_watcher(
    pipe_broken: Arc<Notify>,
    target: Option<self::signal::SignalTarget>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        pipe_broken.notified().await;
        debug!("pipe-broken watcher: downstream consumer closed our output");
        #[cfg(unix)]
        if let Some(t) = target {
            self::signal::forward_signal(t, libc::SIGPIPE);
        }
        #[cfg(not(unix))]
        let _ = target;
    })
}

/// Convert a [`portable_pty::ExitStatus`] into a [`std::process::ExitStatus`]
/// the caller can `.code()` and (on Unix) `.signal()` on.
///
/// portable-pty stores the signal as a *name string* (the output of
/// `strsignal(3)`) and clamps `exit_code()` to `1` when the child died from
/// a signal. If we naively round-tripped through `exit_code()`, every
/// signal-killed child — including the user's Ctrl-C — would surface as
/// exit code `1`, losing the shell-standard `128 + signum` semantics in
/// PTY mode. Map the common signal names back to their `libc` constants so
/// the eventual `propagate_exit_code` call can produce `130` for SIGINT,
/// `143` for SIGTERM, etc.
fn portable_status_to_exit_status(s: &portable_pty::ExitStatus) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(name) = s.signal() {
            if let Some(signum) = signal_name_to_signum(name) {
                // Encode as signal-killed in the wait status: low 7 bits =
                // signum, high byte unused. `ExitStatusExt::from_raw` takes
                // a wait4-style status; this format makes `.signal()`
                // return Some(signum) and `.code()` return None.
                return ExitStatus::from_raw(signum & 0x7f);
            }
            // Unknown signal name (locale-dependent strsignal output) —
            // fall back to "code 1" so the shell at least sees a failure
            // rather than success.
        }
        ExitStatus::from_raw((s.exit_code() as i32) << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(s.exit_code())
    }
}

/// Map portable-pty's `strsignal(3)`-derived signal name back to a libc
/// signum. Returns `None` for names we don't recognise (e.g. unusual
/// locale output, or signals we don't bother enumerating).
///
/// The names listed cover every signal a *normal* wrapped command will
/// die from in real shell use: SIGINT (Ctrl-C), SIGTERM (kill default),
/// SIGHUP (terminal hangup), SIGQUIT (Ctrl-\), SIGKILL (`kill -9`),
/// SIGABRT (panic / abort), SIGBUS / SIGSEGV / SIGFPE (crashes),
/// SIGPIPE (broken pipe), SIGALRM (timeout), SIGUSR1/2 (custom). The
/// mapping accepts both Linux's `strsignal` outputs ("Killed",
/// "Terminated") and macOS's slightly different forms where they differ.
#[cfg(unix)]
fn signal_name_to_signum(name: &str) -> Option<i32> {
    match name {
        "Hangup" => Some(libc::SIGHUP),
        "Interrupt" | "Interrupted" => Some(libc::SIGINT),
        "Quit" => Some(libc::SIGQUIT),
        "Illegal instruction" => Some(libc::SIGILL),
        "Trace/breakpoint trap" | "Trace/BPT trap" => Some(libc::SIGTRAP),
        "Aborted" | "Abort trap" => Some(libc::SIGABRT),
        "Bus error" => Some(libc::SIGBUS),
        "Floating point exception" | "Arithmetic exception" => Some(libc::SIGFPE),
        "Killed" => Some(libc::SIGKILL),
        "User defined signal 1" => Some(libc::SIGUSR1),
        "Segmentation fault" => Some(libc::SIGSEGV),
        "User defined signal 2" => Some(libc::SIGUSR2),
        "Broken pipe" => Some(libc::SIGPIPE),
        "Alarm clock" => Some(libc::SIGALRM),
        "Terminated" => Some(libc::SIGTERM),
        // portable_pty's fallback when strsignal returns null:
        s if s.starts_with("Signal ") => s.strip_prefix("Signal ")?.parse().ok(),
        _ => None,
    }
}

/// Tee a tokio AsyncRead source (used in pipes mode for child stdout/stderr).
///
/// Mirrors [`tee_task`] for the async-read case: forwards the source's
/// bytes to the parent stream, fans out to the scanner, and on a parent
/// `BrokenPipe` notifies `pipe_broken` so the orchestrator can SIGPIPE
/// the child.
async fn async_tee<R>(
    src: R,
    sink: SinkKind,
    scan_tx: mpsc::Sender<(StreamId, Bytes)>,
    drops: Drops,
    pipe_broken: Arc<Notify>,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    match sink {
        SinkKind::Stdout => {
            async_tee_loop(src, tokio::io::stdout(), sink, scan_tx, drops, pipe_broken).await
        }
        SinkKind::Stderr => {
            async_tee_loop(src, tokio::io::stderr(), sink, scan_tx, drops, pipe_broken).await
        }
    }
}

async fn async_tee_loop<R, W>(
    mut src: R,
    mut writer: W,
    sink: SinkKind,
    scan_tx: mpsc::Sender<(StreamId, Bytes)>,
    drops: Drops,
    pipe_broken: Arc<Notify>,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWriteExt + Unpin,
{
    use tokio::io::AsyncReadExt;
    let stream_id: StreamId = sink.into();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match src.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => match writer.write_all(&buf[..n]).await {
                Ok(()) => {
                    if let Err(e) = writer.flush().await {
                        if e.kind() == std::io::ErrorKind::BrokenPipe {
                            debug!(?sink, "async tee: parent flush BrokenPipe");
                            pipe_broken.notify_one();
                            return;
                        }
                    }
                    if scan_tx
                        .try_send((stream_id, Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    debug!(?sink, "async tee: parent write BrokenPipe; halting tee");
                    pipe_broken.notify_one();
                    return;
                }
                Err(e) => {
                    debug!(error = %e, ?sink, "async tee: parent write failed; dropping chunk");
                    continue;
                }
            },
            Err(e) => {
                debug!(error = %e, ?sink, "async tee: read error");
                break;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod conversion_tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn signal_name_to_signum_maps_common_signals() {
        assert_eq!(signal_name_to_signum("Interrupt"), Some(libc::SIGINT));
        assert_eq!(signal_name_to_signum("Interrupted"), Some(libc::SIGINT));
        assert_eq!(signal_name_to_signum("Terminated"), Some(libc::SIGTERM));
        assert_eq!(signal_name_to_signum("Killed"), Some(libc::SIGKILL));
        assert_eq!(signal_name_to_signum("Hangup"), Some(libc::SIGHUP));
        assert_eq!(signal_name_to_signum("Quit"), Some(libc::SIGQUIT));
        assert_eq!(signal_name_to_signum("Aborted"), Some(libc::SIGABRT));
        assert_eq!(signal_name_to_signum("Abort trap"), Some(libc::SIGABRT));
        assert_eq!(signal_name_to_signum("Bus error"), Some(libc::SIGBUS));
        assert_eq!(
            signal_name_to_signum("Segmentation fault"),
            Some(libc::SIGSEGV)
        );
        assert_eq!(signal_name_to_signum("Broken pipe"), Some(libc::SIGPIPE));
        assert_eq!(signal_name_to_signum("Alarm clock"), Some(libc::SIGALRM));
        // portable_pty's fallback when strsignal returns null.
        assert_eq!(signal_name_to_signum("Signal 31"), Some(31));
        // Unknown name returns None so the caller falls back to exit code.
        assert_eq!(signal_name_to_signum("blah blah"), None);
    }

    #[test]
    fn portable_status_to_exit_status_preserves_signal_for_sigint() {
        let portable = portable_pty::ExitStatus::with_signal("Interrupt");
        let status = portable_status_to_exit_status(&portable);
        assert_eq!(
            status.signal(),
            Some(libc::SIGINT),
            "signal-killed PTY child should round-trip to a signalled std::process::ExitStatus"
        );
        assert_eq!(
            status.code(),
            None,
            "signalled status must report code() = None so the caller can apply 128+signum"
        );
    }

    #[test]
    fn portable_status_to_exit_status_preserves_signal_for_sigterm() {
        let portable = portable_pty::ExitStatus::with_signal("Terminated");
        let status = portable_status_to_exit_status(&portable);
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn portable_status_to_exit_status_passes_through_normal_exit() {
        let portable = portable_pty::ExitStatus::with_exit_code(7);
        let status = portable_status_to_exit_status(&portable);
        assert_eq!(status.signal(), None);
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn portable_status_to_exit_status_unknown_signal_falls_back_to_exit_code() {
        // portable-pty puts code=1 in this case (matches std::process behavior).
        let portable = portable_pty::ExitStatus::with_signal("XYZZY signal");
        let status = portable_status_to_exit_status(&portable);
        // Falls back to "exit code" path: code = 1
        assert_eq!(status.code(), Some(1));
    }
}

#[allow(clippy::too_many_arguments)]
async fn scan_consumer_task(
    mut rx: mpsc::Receiver<(StreamId, Bytes)>,
    scanner: Arc<Scanner>,
    patterns: Arc<Vec<Pattern>>,
    debouncers: Arc<Vec<Debouncer>>,
    trigger_client: Arc<TriggerClient>,
    default_backend_id: String,
    last_pattern_fire: Arc<StdMutex<Option<Instant>>>,
) {
    while let Some((stream, chunk)) = rx.recv().await {
        let events = scanner.feed(stream, &chunk);
        if events.is_empty() {
            continue;
        }
        dispatch_events(
            &events,
            &patterns,
            &debouncers,
            &trigger_client,
            &default_backend_id,
            &last_pattern_fire,
        );
    }
}

fn dispatch_events(
    events: &[crate::scan::MatchEvent],
    patterns: &[Pattern],
    debouncers: &[Debouncer],
    trigger_client: &TriggerClient,
    default_backend_id: &str,
    last_pattern_fire: &StdMutex<Option<Instant>>,
) {
    for ev in events {
        let Some(pat) = patterns.get(ev.pattern_idx) else {
            warn!(idx = ev.pattern_idx, "scan: pattern index out of range");
            continue;
        };
        let Some(debouncer) = debouncers.get(ev.pattern_idx) else {
            warn!(idx = ev.pattern_idx, "scan: debouncer index out of range");
            continue;
        };
        match debouncer.check_and_update() {
            Decision::Drop => {
                debug!(
                    pattern = pat.name,
                    line = %ev.line_excerpt,
                    "matched but debounced; dropped"
                );
                continue;
            }
            Decision::Fire => {
                debug!(
                    pattern = pat.name,
                    line = %ev.line_excerpt,
                    "matched; firing trigger"
                );
                let req = build_pattern_trigger(pat, default_backend_id);
                trigger_client.fire(req);
                *last_pattern_fire.lock().expect("last-fire mutex poisoned") = Some(Instant::now());
            }
        }
    }
}
