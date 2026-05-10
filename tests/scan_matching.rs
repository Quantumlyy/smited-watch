//! Pattern scanner: line-buffered RegexSet matching with ANSI stripping.

use std::sync::Arc;

use smited_watch::config::Pattern;
use smited_watch::scan::{Scanner, StreamId, MAX_LINE};

fn pat(name: &str, regex: &str) -> Pattern {
    Pattern {
        name: name.into(),
        regex: regex.into(),
        sensation: format!("{name}_sensation"),
        backend_id: None,
        debounce_ms: 0,
        intensity_scale: None,
        priority: None,
        compiled: None,
    }
}

fn make_scanner(patterns: Vec<Pattern>) -> Scanner {
    Scanner::new(Arc::new(patterns)).expect("scanner should build from valid patterns")
}

#[test]
fn single_line_single_pattern_matches() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    let events = s.feed(StreamId::Stdout, b"error TS1234\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pattern_idx, 0);
    assert_eq!(events[0].stream, StreamId::Stdout);
    assert!(events[0].line_excerpt.contains("error TS1234"));
}

#[test]
fn single_line_multiple_patterns_all_fire() {
    let s = make_scanner(vec![
        pat("ts", r"error TS\d+"),
        pat("any_error", r"error"),
        pat("digits", r"\d+"),
    ]);
    let events = s.feed(StreamId::Stdout, b"error TS1234\n");
    let mut idxs: Vec<usize> = events.iter().map(|e| e.pattern_idx).collect();
    idxs.sort();
    assert_eq!(idxs, vec![0, 1, 2]);
}

#[test]
fn no_match_returns_empty() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(StreamId::Stdout, b"nothing here\n").is_empty());
}

#[test]
fn ansi_escapes_are_stripped_before_matching() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    // Red-coloured "error TS1234" wrapped in ANSI codes.
    let events = s.feed(StreamId::Stdout, b"\x1b[31merror TS1234\x1b[0m\n");
    assert_eq!(events.len(), 1, "ANSI codes must not block the regex");
    assert_eq!(events[0].pattern_idx, 0);
}

#[test]
fn lines_split_across_feed_boundaries_reassemble() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(StreamId::Stdout, b"err").is_empty());
    assert!(s.feed(StreamId::Stdout, b"or T").is_empty());
    let events = s.feed(StreamId::Stdout, b"S1234\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pattern_idx, 0);
}

#[test]
fn multiple_lines_in_one_feed() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    let events = s.feed(StreamId::Stdout, b"error TS1\nokay\nerror TS2\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].pattern_idx, 0);
    assert_eq!(events[1].pattern_idx, 0);
}

#[test]
fn final_partial_line_emerges_on_flush() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(
        s.feed(StreamId::Stdout, b"error TS9").is_empty(),
        "no newline yet — buffered, no event"
    );
    let events = s.flush(StreamId::Stdout);
    assert_eq!(events.len(), 1, "flush must emit the trailing partial line");
    assert_eq!(events[0].pattern_idx, 0);
}

#[test]
fn empty_lines_are_scanned_but_dont_produce_matches() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(StreamId::Stdout, b"\n\n\n").is_empty());
}

#[test]
fn force_flush_at_max_line_boundary() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    // Build a giant single line (no newline) larger than MAX_LINE that
    // contains a match somewhere in the first MAX_LINE bytes — it MUST be
    // emitted via the force-flush path so we don't OOM on a `tail -f` of a
    // log with no newlines.
    let mut chunk = vec![b'.'; MAX_LINE - 12];
    chunk.extend_from_slice(b"error TS9999");
    let events = s.feed(StreamId::Stdout, &chunk);
    assert_eq!(
        events.len(),
        1,
        "scanner must force-flush at MAX_LINE and emit the (possibly truncated) line"
    );
    assert_eq!(events[0].pattern_idx, 0);

    // Continue feeding more bytes of the same logical line (still no
    // newline) — those must NOT re-fire the match because the flush
    // discarded the previous content.
    let more = vec![b'.'; 1024];
    assert!(
        s.feed(StreamId::Stdout, &more).is_empty(),
        "post-flush continuation should not re-match the already-emitted line"
    );
}

#[test]
fn carriage_return_alone_is_not_a_line_terminator() {
    // Many tools redraw a line by emitting `\r…` without a newline. The
    // scanner must NOT treat `\r` as a line terminator (otherwise a single
    // spinner update would produce dozens of fake match attempts).
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(StreamId::Stdout, b"\rerror TS1234").is_empty());
    let events = s.feed(StreamId::Stdout, b"\n");
    assert_eq!(events.len(), 1);
}

#[test]
fn invalid_pattern_regex_at_construction_errors_with_name() {
    let mut p = pat("broken", "(unclosed");
    p.regex = "(unclosed".into();
    let err = Scanner::new(Arc::new(vec![p])).expect_err("bad regex should error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("broken"),
        "error should mention pattern name, got: {msg}"
    );
}

/// Stdout's partial line (no newline yet) and stderr's bytes must NOT be
/// stitched together into a single logical line. Pre-fix the scanner had
/// one shared buffer, so a stdout chunk "error TS" followed by a stderr
/// chunk "1234\n" would falsely match `error TS\d+`. Post-fix the
/// per-stream buffers keep them separate: stdout's prefix stays buffered
/// until stdout itself emits a newline, and stderr's "1234" alone
/// doesn't match.
#[test]
fn stdout_and_stderr_chunks_do_not_splice_into_one_logical_line() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(
        s.feed(StreamId::Stdout, b"error TS").is_empty(),
        "stdout prefix without newline buffers but doesn't match"
    );
    let stderr_events = s.feed(StreamId::Stderr, b"1234\n");
    assert!(
        stderr_events.is_empty(),
        "stderr emitting '1234\\n' on its own (with no 'error TS' prefix) \
         must NOT match — got {stderr_events:?}"
    );
    // And the stdout prefix is still cleanly buffered for later, undisturbed.
    let stdout_events = s.feed(StreamId::Stdout, b"5678\n");
    assert_eq!(
        stdout_events.len(),
        1,
        "stdout's own newline triggers its match"
    );
    assert_eq!(stdout_events[0].pattern_idx, 0);
    assert_eq!(stdout_events[0].stream, StreamId::Stdout);
    assert!(
        stdout_events[0].line_excerpt.contains("error TS5678"),
        "match should be against stdout's own line, not the spliced one; got {}",
        stdout_events[0].line_excerpt
    );
}

/// PR review #3 (Copilot): the MAX_LINE force-flush must drain
/// `MAX_LINE` bytes per iteration, not the entire buffer. If a single
/// `feed` call delivers `>= 2 * MAX_LINE` bytes with no newline, a
/// `split_to(buf.len())` would scan-and-allocate the whole thing in
/// one go, defeating the per-line cap.
///
/// We can observe the per-iteration bound by placing two matches:
/// one straddling the MAX_LINE boundary (which the bounded drain will
/// split and fail to match), and one fully contained in the second
/// MAX_LINE region (which still matches). The pre-fix behaviour would
/// have produced 2 events from a single feed; the post-fix behaviour
/// produces exactly 1.
#[test]
fn force_flush_drains_in_max_line_chunks_so_buffer_is_truly_bounded() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);

    // Build a chunk of exactly 2 * MAX_LINE bytes:
    //
    //   bytes 0 .. MAX_LINE-8           dots (filler)
    //   bytes MAX_LINE-8 .. MAX_LINE    "error TS"   ← straddling match starts here
    //   bytes MAX_LINE .. MAX_LINE+4    "1234"       ← straddling match's digits
    //   bytes MAX_LINE+4 .. MAX_LINE+16 "error TS5678"  ← contained match (kept
    //                                                     near the start of the
    //                                                     second chunk so the
    //                                                     excerpt includes it)
    //   bytes MAX_LINE+16 .. 2*MAX_LINE dots (trailing filler)
    let mut chunk = Vec::with_capacity(2 * MAX_LINE);
    chunk.extend(std::iter::repeat_n(b'.', MAX_LINE - 8));
    chunk.extend_from_slice(b"error TS");
    chunk.extend_from_slice(b"1234");
    chunk.extend_from_slice(b"error TS5678");
    chunk.extend(std::iter::repeat_n(b'.', MAX_LINE - 16));
    assert_eq!(
        chunk.len(),
        2 * MAX_LINE,
        "test setup: chunk should be exactly 2 * MAX_LINE"
    );

    let events = s.feed(StreamId::Stdout, &chunk);
    assert_eq!(
        events.len(),
        1,
        "expected exactly 1 match — the contained 'error TS5678' fires; \
         the straddling 'error TS1234' is split at the MAX_LINE boundary \
         by the bounded force-flush. Pre-fix would have scanned both \
         matches in one buffer-spanning scan_line call. got {events:?}"
    );
    assert!(events[0].line_excerpt.contains("5678"));
}

/// The default test_failure pattern (in examples/wrap.toml + the
/// patterns-cookbook entry) must NOT match a clean test summary like
/// "0 failed, 12 passed", which is what every major runner prints on
/// a green run. The leading `[1-9]\d*` (instead of `\d+`) is what
/// keeps the regex narrowly targeted at *real* failure counts.
#[test]
fn test_failure_pattern_excludes_zero_count() {
    let s = make_scanner(vec![pat(
        "generic_test_failure",
        r"(?i)[1-9]\d*\s+(failed|failing)",
    )]);

    // No-fire cases: clean test runs.
    for clean in [
        "Tests:       0 failed, 12 passed\n",
        "0 failing\n",
        "Tests: 0 failed\n",
        "test result: ok. 5 passed; 0 failed; 0 ignored\n",
    ] {
        assert!(
            s.feed(StreamId::Stdout, clean.as_bytes()).is_empty(),
            "clean line {clean:?} must NOT match the test-failure pattern"
        );
    }

    // Fire cases: actual failures.
    for fail in [
        "Tests:       1 failed, 11 passed\n",
        "12 failing\n",
        "Tests:       100 failed\n",
    ] {
        let events = s.feed(StreamId::Stdout, fail.as_bytes());
        assert!(
            !events.is_empty(),
            "real failure {fail:?} should match the test-failure pattern"
        );
        assert_eq!(events[0].pattern_idx, 0);
    }
}

/// Both streams have buffered partial lines on EOF; flush_all must drain
/// each independently.
#[test]
fn flush_all_emits_trailing_lines_for_every_stream() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(StreamId::Stdout, b"error TS1").is_empty());
    assert!(s.feed(StreamId::Stderr, b"error TS2").is_empty());
    let events = s.flush_all();
    assert_eq!(events.len(), 2);
    let mut by_stream: std::collections::HashMap<StreamId, &str> = std::collections::HashMap::new();
    for ev in &events {
        by_stream.insert(ev.stream, &ev.line_excerpt);
    }
    assert!(by_stream
        .get(&StreamId::Stdout)
        .is_some_and(|s| s.contains("error TS1")));
    assert!(by_stream
        .get(&StreamId::Stderr)
        .is_some_and(|s| s.contains("error TS2")));
}
