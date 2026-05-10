//! Pattern scanner over a byte stream.
//!
//! Builds a single [`regex::RegexSet`] from every configured pattern so the
//! per-line scan cost is one pass over the line regardless of how many
//! patterns the user has. Bytes are fed via [`Scanner::feed`], which
//! accumulates into a rolling buffer; each complete `\n`-delimited line is
//! ANSI-stripped and matched against the set, returning a
//! [`MatchEvent`] per matched pattern.
//!
//! ## Why ANSI stripping per line and not on the whole stream
//!
//! Build tools emit colour codes everywhere — `\x1b[31merror TS1234\x1b[0m`
//! — and the regex `error TS\d+` would never match the raw bytes. Stripping
//! per line is cheap (the `strip-ansi-escapes` crate is a state machine on
//! bytes) and lets us keep the *passthrough* stream byte-perfect.
//!
//! ## The MAX_LINE force-flush
//!
//! A pathological input — `tail -f` on a log with no newlines, a hung
//! progress indicator that emits megabytes between newlines — would grow
//! the buffer without bound. We cap each line at [`MAX_LINE`]; on overflow
//! we force-flush the buffered prefix as if a newline had arrived, log a
//! debug-level warning, and start a fresh buffer. The match may be against
//! a truncated line, but that's better than OOM and far better than
//! silently dropping the line.
//!
//! ## Why `\r` is not a line terminator
//!
//! Many tools re-paint a line by emitting `\r<new content>` without a
//! newline (progress spinners, percentage counters). Treating `\r` as a
//! terminator would produce N fake "lines" per spinner tick. The scanner
//! only splits on `\n`.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use bytes::BytesMut;
use regex::RegexSet;
use tracing::debug;

use crate::config::Pattern;

/// Maximum line length the scanner will buffer before forcing a flush.
pub const MAX_LINE: usize = 64 * 1024;

/// Excerpt length included in [`MatchEvent::line_excerpt`] for log/trace
/// purposes. Chosen to keep log lines readable when a match lands in the
/// middle of a long compiler error.
const EXCERPT_MAX: usize = 512;

/// Which of the wrapped command's output streams a chunk came from.
///
/// The scanner keeps a *separate* line buffer per stream so a partial
/// stdout line that hasn't yet seen its `\n` can't get spliced together
/// with bytes that arrive on stderr — splicing would let the regex
/// match a line the child never actually emitted. In PTY mode there is
/// only one stream; the orchestrator labels it [`StreamId::Stdout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamId {
    Stdout = 0,
    Stderr = 1,
}

/// Number of distinct streams the scanner buffers. Bumping this requires
/// matching changes to [`StreamId`] and the per-stream array indexing.
const STREAM_COUNT: usize = 2;

/// One pattern matching one line.
#[derive(Debug, Clone)]
pub struct MatchEvent {
    /// Index into the `patterns` vector the [`Scanner`] was built with.
    pub pattern_idx: usize,
    /// Which child stream the matching line came from.
    pub stream: StreamId,
    /// First [`EXCERPT_MAX`] bytes of the matching line (UTF-8-replaced),
    /// for log output. The line is ANSI-stripped before being captured.
    pub line_excerpt: String,
}

/// Stateful scanner: holds the [`RegexSet`] and *per-stream* rolling
/// line buffers.
#[derive(Debug)]
pub struct Scanner {
    set: RegexSet,
    /// Owned by the caller too — we hold an `Arc` for cheap cloning into
    /// dispatch tasks that need to look up `Pattern` metadata by index.
    #[allow(dead_code)]
    patterns: Arc<Vec<Pattern>>,
    /// Indexed by `StreamId as usize`.
    bufs: Mutex<[BytesMut; STREAM_COUNT]>,
}

impl Scanner {
    /// Build a scanner from the configured patterns.
    ///
    /// Errors with the pattern's `name` if any regex is invalid — this
    /// duplicates [`crate::config::load`]'s validation but lets the scanner
    /// be constructed from synthetic test patterns that bypassed the config
    /// loader.
    pub fn new(patterns: Arc<Vec<Pattern>>) -> Result<Self> {
        // Validate each regex individually first so we can attribute the
        // error to a specific pattern by name. RegexSet::new returns a
        // single error pointing at "the set", which is less useful.
        for p in patterns.iter() {
            regex::Regex::new(&p.regex)
                .with_context(|| format!("pattern {:?}: invalid regex {:?}", p.name, p.regex))?;
        }
        let set = RegexSet::new(patterns.iter().map(|p| &p.regex)).context("build RegexSet")?;
        Ok(Self {
            set,
            patterns,
            bufs: Mutex::new([BytesMut::new(), BytesMut::new()]),
        })
    }

    /// Feed bytes from one of the wrapped command's output streams.
    ///
    /// Returns one [`MatchEvent`] per matching `(line, pattern)` pair.
    /// Lines that don't match any pattern produce no events.
    ///
    /// Bytes go into the buffer for `stream`, never crossing over to
    /// the other stream's buffer. Lines longer than [`MAX_LINE`] are
    /// force-flushed, possibly truncated; a debug-level log records
    /// the truncation.
    pub fn feed(&self, stream: StreamId, bytes: &[u8]) -> Vec<MatchEvent> {
        let mut events = Vec::new();
        let mut bufs = self.bufs.lock().expect("scanner buffer poisoned");
        let buf = &mut bufs[stream as usize];
        buf.extend_from_slice(bytes);
        loop {
            // Drain any complete `\n`-delimited lines first.
            if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: BytesMut = buf.split_to(nl + 1);
                // Drop the trailing newline. Some platforms also emit `\r\n`.
                let mut content: &[u8] = &line[..line.len() - 1];
                if content.last() == Some(&b'\r') {
                    content = &content[..content.len() - 1];
                }
                self.scan_line(stream, content, &mut events);
                continue;
            }
            // No newline in the buffer. Force-flush if oversized.
            let len = buf.len();
            if len >= MAX_LINE {
                let drained = buf.split_to(len);
                debug!(
                    line_len = drained.len(),
                    ?stream,
                    "scanner: force-flushing line at MAX_LINE without newline"
                );
                self.scan_line(stream, &drained, &mut events);
                continue;
            }
            break;
        }
        events
    }

    /// Drain any trailing partial line for one stream (no newline yet).
    pub fn flush(&self, stream: StreamId) -> Vec<MatchEvent> {
        let mut bufs = self.bufs.lock().expect("scanner buffer poisoned");
        let buf = &mut bufs[stream as usize];
        let len = buf.len();
        if len == 0 {
            return Vec::new();
        }
        let drained = buf.split_to(len);
        let mut events = Vec::new();
        self.scan_line(stream, &drained, &mut events);
        events
    }

    /// Drain trailing partial lines for *every* stream. Used at shutdown
    /// after both reader pipelines have been drained.
    pub fn flush_all(&self) -> Vec<MatchEvent> {
        let mut events = self.flush(StreamId::Stdout);
        events.extend(self.flush(StreamId::Stderr));
        events
    }

    fn scan_line(&self, stream: StreamId, content: &[u8], out: &mut Vec<MatchEvent>) {
        let stripped = strip_ansi_escapes::strip(content);
        // RegexSet operates on &str; lossily convert so non-UTF-8 bytes
        // don't sink the scan.
        let s = String::from_utf8_lossy(&stripped);
        let matches = self.set.matches(&s);
        if !matches.matched_any() {
            return;
        }
        let excerpt = make_excerpt(&s);
        for idx in matches.iter() {
            out.push(MatchEvent {
                pattern_idx: idx,
                stream,
                line_excerpt: excerpt.clone(),
            });
        }
    }
}

fn make_excerpt(line: &str) -> String {
    if line.len() <= EXCERPT_MAX {
        return line.to_string();
    }
    // Cut at a char boundary <= EXCERPT_MAX so we don't slice through a
    // multi-byte UTF-8 sequence.
    let mut cut = EXCERPT_MAX;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &line[..cut])
}
