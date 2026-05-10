//! Tee a child stream: write byte-perfect to the parent's stdout/stderr
//! AND fan out a copy to the scanner.
//!
//! ## Design
//!
//! `portable-pty` exposes the master PTY end as `std::io::Read` (sync). To
//! bridge it into the tokio runtime we run a dedicated OS thread per
//! readable stream that loops `read(buf)` and pushes each chunk into a
//! bounded `mpsc::Sender<Bytes>`. The async tee task pulls from the
//! receiver and:
//!
//! 1. **Writes byte-perfect to the parent terminal first.** This applies
//!    natural back-pressure on the child via the OS pipe/PTY buffer — if
//!    the user's terminal is slow, the child stops emitting until we catch
//!    up, exactly as it would without the watcher.
//! 2. **Fans out the same chunk to the scanner via `try_send`.** If the
//!    scanner channel is full (the scanner is falling behind), the chunk
//!    is dropped on the floor and a counter is incremented. This is a
//!    deliberate trade-off: passthrough latency matters more than scanner
//!    completeness. A user staring at the build output should never see
//!    stutter because pattern-matching couldn't keep up.
//!
//! ## Channel sizing
//!
//! 1024 chunks of typical 4-8 KiB each gives ~8 MiB of headroom — enough
//! to swallow pretty much any realistic burst from a build tool without
//! dropping. If we ever do drop, [`Drops`] tracks the count and the
//! shutdown summary logs it at debug level.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::debug;

use crate::scan::StreamId;

/// Maximum buffered chunks between the blocking reader thread and the
/// async tee task, and between the tee task and the scanner consumer.
pub const CHANNEL_CAPACITY: usize = 1024;

/// Maximum bytes per `read` call from the child. Sized to balance syscall
/// frequency against latency for spinner-style outputs.
const READ_BUF: usize = 8 * 1024;

/// Which parent stream this teed copy goes to.
#[derive(Debug, Clone, Copy)]
pub enum SinkKind {
    Stdout,
    Stderr,
}

impl From<SinkKind> for StreamId {
    fn from(s: SinkKind) -> Self {
        match s {
            SinkKind::Stdout => StreamId::Stdout,
            SinkKind::Stderr => StreamId::Stderr,
        }
    }
}

/// Counter shared between the tee task and the orchestrator's shutdown
/// summary. Incremented every time the scanner channel was full and we
/// dropped a chunk on the floor.
pub type Drops = Arc<AtomicU64>;

/// Spawn an OS thread that reads from `reader` in a loop and pushes each
/// chunk into `tx`. Returns the join handle so the orchestrator can wait
/// on it during shutdown.
///
/// The thread exits cleanly on:
/// * EOF from the reader (returns 0 bytes)
/// * The receiver being dropped (mpsc send fails)
/// * Any non-EOF read error (logged at debug)
pub fn spawn_blocking_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Bytes>,
) -> JoinHandle<()> {
    std::thread::spawn(move || loop {
        let mut buf = vec![0u8; READ_BUF];
        let n = match reader.read(&mut buf) {
            Ok(0) => return, // EOF
            Ok(n) => n,
            Err(e) => {
                // ErrorKind::Interrupted on EINTR is benign — retry; on
                // anything else, the stream is gone, treat as EOF.
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                debug!(error = %e, "tee reader: stream error, exiting");
                return;
            }
        };
        buf.truncate(n);
        if tx.blocking_send(Bytes::from(buf)).is_err() {
            // Receiver dropped; orchestrator shut down. Stop reading.
            return;
        }
    })
}

/// Async task: pull chunks from `rx`, write them to the parent stream
/// (Stdout or Stderr), then fan out a copy to `scan_tx` tagged with the
/// originating [`StreamId`] so the scanner buffers them separately.
///
/// On `scan_tx` being full, the chunk is dropped and `drops` is
/// incremented; the chunk has already been written to the parent stream
/// at that point so the user sees no degradation.
pub async fn tee_task(
    rx: mpsc::Receiver<Bytes>,
    sink: SinkKind,
    scan_tx: mpsc::Sender<(StreamId, Bytes)>,
    drops: Drops,
) {
    // tokio::io::Stdout and Stderr have different concrete types — we
    // monomorphise on each side rather than try to dyn-trait them, since
    // `AsyncWriteExt` isn't object-safe.
    match sink {
        SinkKind::Stdout => tee_loop(rx, tokio::io::stdout(), sink, scan_tx, drops).await,
        SinkKind::Stderr => tee_loop(rx, tokio::io::stderr(), sink, scan_tx, drops).await,
    }
}

async fn tee_loop<W>(
    mut rx: mpsc::Receiver<Bytes>,
    mut writer: W,
    sink: SinkKind,
    scan_tx: mpsc::Sender<(StreamId, Bytes)>,
    drops: Drops,
) where
    W: AsyncWriteExt + Unpin,
{
    let stream_id: StreamId = sink.into();
    while let Some(chunk) = rx.recv().await {
        // Write to parent first — back-pressure on slow terminal naturally
        // throttles the child via OS buffer fill.
        if let Err(e) = writer.write_all(&chunk).await {
            debug!(error = %e, ?sink, "tee: parent write failed; dropping chunk");
            // Continue draining rx so the reader thread can still exit.
            continue;
        }
        if let Err(e) = writer.flush().await {
            debug!(error = %e, ?sink, "tee: parent flush failed");
        }
        // Then fan out to scanner. Drop on full — passthrough latency wins.
        if scan_tx.try_send((stream_id, chunk)).is_err() {
            drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}
