//! Pattern scanner: line-buffered RegexSet matching with ANSI stripping.

use std::sync::Arc;

use smited_watch::config::Pattern;
use smited_watch::scan::{Scanner, MAX_LINE};

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
    let events = s.feed(b"error TS1234\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pattern_idx, 0);
    assert!(events[0].line_excerpt.contains("error TS1234"));
}

#[test]
fn single_line_multiple_patterns_all_fire() {
    let s = make_scanner(vec![
        pat("ts", r"error TS\d+"),
        pat("any_error", r"error"),
        pat("digits", r"\d+"),
    ]);
    let events = s.feed(b"error TS1234\n");
    let mut idxs: Vec<usize> = events.iter().map(|e| e.pattern_idx).collect();
    idxs.sort();
    assert_eq!(idxs, vec![0, 1, 2]);
}

#[test]
fn no_match_returns_empty() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(b"nothing here\n").is_empty());
}

#[test]
fn ansi_escapes_are_stripped_before_matching() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    // Red-coloured "error TS1234" wrapped in ANSI codes.
    let events = s.feed(b"\x1b[31merror TS1234\x1b[0m\n");
    assert_eq!(events.len(), 1, "ANSI codes must not block the regex");
    assert_eq!(events[0].pattern_idx, 0);
}

#[test]
fn lines_split_across_feed_boundaries_reassemble() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(b"err").is_empty());
    assert!(s.feed(b"or T").is_empty());
    let events = s.feed(b"S1234\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pattern_idx, 0);
}

#[test]
fn multiple_lines_in_one_feed() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    let events = s.feed(b"error TS1\nokay\nerror TS2\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].pattern_idx, 0);
    assert_eq!(events[1].pattern_idx, 0);
}

#[test]
fn final_partial_line_emerges_on_flush() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(
        s.feed(b"error TS9").is_empty(),
        "no newline yet — buffered, no event"
    );
    let events = s.flush();
    assert_eq!(events.len(), 1, "flush must emit the trailing partial line");
    assert_eq!(events[0].pattern_idx, 0);
}

#[test]
fn empty_lines_are_scanned_but_dont_produce_matches() {
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(b"\n\n\n").is_empty());
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
    let events = s.feed(&chunk);
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
        s.feed(&more).is_empty(),
        "post-flush continuation should not re-match the already-emitted line"
    );
}

#[test]
fn carriage_return_alone_is_not_a_line_terminator() {
    // Many tools redraw a line by emitting `\r…` without a newline. The
    // scanner must NOT treat `\r` as a line terminator (otherwise a single
    // spinner update would produce dozens of fake match attempts).
    let s = make_scanner(vec![pat("ts", r"error TS\d+")]);
    assert!(s.feed(b"\rerror TS1234").is_empty());
    let events = s.feed(b"\n");
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
