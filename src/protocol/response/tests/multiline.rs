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

#[test]
fn test_has_spanning_terminator_all_splits() {
    // Test all 4 possible split positions of the 5-byte terminator "\r\n.\r\n"

    // Split after byte 1: tail="\r", current="\n.\r\n"
    assert!(NntpResponse::has_spanning_terminator(
        b"some data\r",
        10,
        b"\n.\r\n",
        4
    ));

    // Split after byte 2: tail="\r\n", current=".\r\n"
    assert!(NntpResponse::has_spanning_terminator(
        b"some data\r\n",
        11,
        b".\r\n",
        3
    ));

    // Split after byte 3: tail="\r\n.", current="\r\n"
    assert!(NntpResponse::has_spanning_terminator(
        b"some data\r\n.",
        12,
        b"\r\n",
        2
    ));

    // Split after byte 4: tail="\r\n.\r", current="\n"
    assert!(NntpResponse::has_spanning_terminator(
        b"some data\r\n.\r",
        13,
        b"\n",
        1
    ));
}

#[test]
fn test_has_spanning_terminator_false_positives() {
    // These should NOT match - they're not the terminator

    // tail="\r", current="\n.X\n" (not terminator)
    assert!(!NntpResponse::has_spanning_terminator(
        b"data\r", 5, b"\n.X\n", 4
    ));

    // tail="\r\n", current=".X\n" (not terminator)
    assert!(!NntpResponse::has_spanning_terminator(
        b"data\r\n",
        6,
        b".X\n",
        3
    ));

    // tail="\r\n.", current="\rX" (not terminator)
    assert!(!NntpResponse::has_spanning_terminator(
        b"data\r\n.",
        7,
        b"\rX",
        2
    ));

    // tail="\r\n.\r", current="X" (not terminator)
    assert!(!NntpResponse::has_spanning_terminator(
        b"data\r\n.\r",
        8,
        b"X",
        1
    ));

    // Current too long - terminator would be in one chunk
    assert!(!NntpResponse::has_spanning_terminator(
        b"data\r",
        5,
        b"\n.\r\nmore",
        8
    ));

    // Tail too short
    assert!(!NntpResponse::has_spanning_terminator(
        b"d", 1, b"\n.\r\n", 4
    ));

    // Empty current
    assert!(!NntpResponse::has_spanning_terminator(
        b"data\r\n.\r",
        8,
        b"",
        0
    ));
}

#[test]
fn test_has_spanning_terminator_edge_cases() {
    // Exactly the right split with minimal tail
    assert!(NntpResponse::has_spanning_terminator(
        b"\r", 1, b"\n.\r\n", 4
    ));

    // Longer tail but correct terminator at end
    assert!(NntpResponse::has_spanning_terminator(
        b"lots of previous data here\r\n.\r",
        30,
        b"\n",
        1
    ));

    // Wrong pattern - looks like terminator but isn't
    assert!(!NntpResponse::has_spanning_terminator(
        b".\r\n", 3, b"\r\n", 2
    )); // This would be ".\r\n" + "\r\n" = wrong

    // Partial matches that aren't terminators
    assert!(!NntpResponse::has_spanning_terminator(
        b"\n", 1, b".\r\n", 3
    )); // "\n" + ".\r\n" = wrong (needs \r\n at start)

    assert!(!NntpResponse::has_spanning_terminator(
        b"\r\n.\n", 4, b"\n", 1
    )); // Wrong - has \n instead of \r before final \n
}

#[test]
fn test_has_spanning_terminator_realistic_scenarios() {
    // Scenario 1: Article data split across network packets
    // Previous packet ends mid-terminator
    let tail = b"Last line of article\r\n.";
    assert!(NntpResponse::has_spanning_terminator(
        tail,
        tail.len(),
        b"\r\n",
        2
    ));

    // Scenario 2: Very small current chunk
    let tail = b"End of response\r\n.\r";
    assert!(NntpResponse::has_spanning_terminator(
        tail,
        tail.len(),
        b"\n",
        1
    ));

    // Scenario 3: Not a terminator - regular data with dot
    let tail = b"Line with .dot\r";
    assert!(!NntpResponse::has_spanning_terminator(
        tail,
        tail.len(),
        b"\nNext line\r\n",
        12
    ));

    // Scenario 4: Complete terminator in current chunk (should return false)
    let tail = b"Data here";
    assert!(!NntpResponse::has_spanning_terminator(
        tail,
        tail.len(),
        b"\r\n.\r\n",
        5
    ));
}
