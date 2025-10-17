//! Tests for multiline response handling

use crate::protocol::response::*;

#[test]
fn test_is_multiline_response() {
    assert!(NntpResponse::is_multiline_response(100));
    assert!(NntpResponse::is_multiline_response(215)); // LIST
    assert!(NntpResponse::is_multiline_response(220)); // ARTICLE
    assert!(NntpResponse::is_multiline_response(221)); // HEAD
    assert!(NntpResponse::is_multiline_response(222)); // BODY
    assert!(!NntpResponse::is_multiline_response(200)); // Greeting
    assert!(!NntpResponse::is_multiline_response(381)); // Auth required
    assert!(!NntpResponse::is_multiline_response(500)); // Error
}

#[test]
fn test_has_multiline_terminator() {
    assert!(NntpResponse::has_multiline_terminator(
        b"data\r\nmore data\r\n.\r\n"
    ));
    assert!(NntpResponse::has_multiline_terminator(
        b"single line\n.\r\n"
    ));
    assert!(!NntpResponse::has_multiline_terminator(b"incomplete\r\n"));
    assert!(!NntpResponse::has_multiline_terminator(b".\r\n")); // Too short
}

#[test]
fn test_multiline_terminator_variations() {
    // Standard terminator
    assert!(NntpResponse::has_multiline_terminator(
        b"line1\r\nline2\r\n.\r\n"
    ));

    // Terminator with just LF
    assert!(NntpResponse::has_multiline_terminator(b"data\n.\r\n"));

    // No terminator
    assert!(!NntpResponse::has_multiline_terminator(
        b"line1\r\nline2\r\n"
    ));

    // Terminator in middle (should not match - ends_with check)
    assert!(!NntpResponse::has_multiline_terminator(b".\r\nmore data"));

    // Just the terminator (exactly 5 bytes)
    assert!(NntpResponse::has_multiline_terminator(b"\r\n.\r\n"));
    // This is only 4 bytes, so should be false
    assert!(!NntpResponse::has_multiline_terminator(b"\n.\r\n"));

    // Too short
    assert!(!NntpResponse::has_multiline_terminator(b".\r\n"));
    assert!(!NntpResponse::has_multiline_terminator(b"abc"));
    assert!(!NntpResponse::has_multiline_terminator(b""));
}

#[test]
fn test_multiline_response_categories() {
    // 1xx informational (all multiline)
    assert!(NntpResponse::is_multiline_response(100));
    assert!(NntpResponse::is_multiline_response(150));
    assert!(NntpResponse::is_multiline_response(199));

    // 2xx specific multiline codes
    assert!(NntpResponse::is_multiline_response(215)); // LIST
    assert!(NntpResponse::is_multiline_response(220)); // ARTICLE
    assert!(NntpResponse::is_multiline_response(221)); // HEAD
    assert!(NntpResponse::is_multiline_response(222)); // BODY
    assert!(NntpResponse::is_multiline_response(224)); // OVER
    assert!(NntpResponse::is_multiline_response(225)); // HDR
    assert!(NntpResponse::is_multiline_response(230)); // NEWNEWS
    assert!(NntpResponse::is_multiline_response(231)); // NEWGROUPS

    // 2xx non-multiline codes
    assert!(!NntpResponse::is_multiline_response(200)); // Greeting
    assert!(!NntpResponse::is_multiline_response(205)); // Goodbye
    assert!(!NntpResponse::is_multiline_response(211)); // Group selected
    assert!(!NntpResponse::is_multiline_response(281)); // Auth accepted

    // 3xx, 4xx, 5xx are not multiline
    assert!(!NntpResponse::is_multiline_response(381));
    assert!(!NntpResponse::is_multiline_response(430));
    assert!(!NntpResponse::is_multiline_response(500));
}
