//! RFC 3977 Section 3.1.1 - Multi-line Response and Byte-Stuffing Tests
//!
//! These tests verify compliance with NNTP multi-line response requirements:
//! - Multi-line blocks are terminated by "\r\n.\r\n"
//! - Lines starting with "." must be dot-stuffed (prepend another ".")
//! - When receiving, initial dots followed by non-CRLF are disregarded
//!
//! Adapted from nntp-rs RFC 3977 multiline.rs tests.

use nntp_proxy::protocol::{CRLF, MULTILINE_TERMINATOR};

/// Helper function to simulate byte-stuffing removal (dot-unstuffing)
///
/// Per RFC 3977 §3.1.1: "If any line of the data block begins with the
/// termination octet followed by one or more octets, the first octet of
/// the line is removed."
fn unstuff_line(line: &str) -> &str {
    if line.starts_with("..") {
        &line[1..]
    } else {
        line
    }
}

/// Helper function to check if a line is the multiline terminator
fn is_terminator(line: &str) -> bool {
    line == "."
}

/// Simulate parsing a multi-line response body (line-based)
fn parse_multiline_body(lines: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    for line in lines {
        if is_terminator(line) {
            break;
        }
        result.push(unstuff_line(line).to_string());
    }
    result
}

/// Parse a raw CRLF-delimited multiline response
fn parse_raw_multiline_response(raw: &str) -> Vec<String> {
    let lines: Vec<&str> = raw.split("\r\n").filter(|s| !s.is_empty()).collect();
    let mut result = Vec::new();
    for line in lines {
        if is_terminator(line) {
            break;
        }
        result.push(unstuff_line(line).to_string());
    }
    result
}

// === Byte-Stuffing Tests (RFC 3977 §3.1.1) ===

#[test]
fn test_dot_stuffing_single_dot_is_terminator() {
    assert!(is_terminator("."));
}

#[test]
fn test_dot_stuffing_double_dot_becomes_single() {
    // RFC 3977 §3.1.1: ".." at start of line becomes "."
    assert_eq!(unstuff_line(".."), ".");
}

#[test]
fn test_dot_stuffing_triple_dot_becomes_double() {
    assert_eq!(unstuff_line("..."), "..");
}

#[test]
fn test_dot_stuffing_quad_dot_becomes_triple() {
    assert_eq!(unstuff_line("...."), "...");
}

#[test]
fn test_dot_stuffing_double_dot_with_text() {
    assert_eq!(unstuff_line("..Hello"), ".Hello");
}

#[test]
fn test_dot_stuffing_preserves_non_dot_lines() {
    assert_eq!(unstuff_line("Hello World"), "Hello World");
    assert_eq!(unstuff_line(""), "");
    assert_eq!(unstuff_line("Normal line"), "Normal line");
}

#[test]
fn test_dot_stuffing_dot_in_middle_unchanged() {
    let line = "Hello.World.Test";
    assert_eq!(unstuff_line(line), line);
}

#[test]
fn test_dot_stuffing_dot_at_end_unchanged() {
    let line = "End with dot.";
    assert_eq!(unstuff_line(line), line);
}

#[test]
fn test_dot_stuffing_single_dot_not_unstuffed() {
    // A single "." is the terminator, not data
    // The is_terminator check happens first in real parsing
    assert_eq!(unstuff_line("."), ".");
    assert!(is_terminator("."));
}

#[test]
fn test_dot_stuffing_with_leading_spaces() {
    // Dot-stuffing only applies if "." is at position 0
    let line = " ..test";
    assert_eq!(unstuff_line(line), " ..test");
}

#[test]
fn test_dot_stuffing_only_first_dot_pair() {
    // Only the FIRST dot is removed
    assert_eq!(unstuff_line("..test.."), ".test..");
}

// === Multi-line Block Parsing ===

#[test]
fn test_multiline_simple_body() {
    let lines = ["Line 1", "Line 2", "Line 3", "."];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 3);
    assert_eq!(result[0], "Line 1");
    assert_eq!(result[1], "Line 2");
    assert_eq!(result[2], "Line 3");
}

#[test]
fn test_multiline_with_dot_stuffed_lines() {
    let lines = ["Normal line", "..This started with a dot", "...", "."];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 3);
    assert_eq!(result[0], "Normal line");
    assert_eq!(result[1], ".This started with a dot");
    assert_eq!(result[2], "..");
}

#[test]
fn test_multiline_empty_lines_preserved() {
    let lines = ["First", "", "Third", "", "."];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 4);
    assert_eq!(result[0], "First");
    assert_eq!(result[1], "");
    assert_eq!(result[2], "Third");
    assert_eq!(result[3], "");
}

#[test]
fn test_multiline_only_terminator() {
    let lines = ["."];
    let result = parse_multiline_body(&lines);
    assert!(result.is_empty());
}

#[test]
fn test_multiline_article_simulation() {
    let lines = [
        "From: user@example.com",
        "Subject: Test Article",
        "Date: Mon, 1 Jan 2024 00:00:00 +0000",
        "Message-ID: <test@example.com>",
        "", // Header/body separator
        "This is the body of the article.",
        "It has multiple lines.",
        "",
        "..Hidden dot line", // Dot-stuffed
        ".",
    ];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 9);
    assert_eq!(result[0], "From: user@example.com");
    assert_eq!(result[4], ""); // Empty separator
    assert_eq!(result[8], ".Hidden dot line"); // Dot-unstuffed
}

#[test]
fn test_multiline_dot_only_line_after_content() {
    // If content has a line that's just ".", server sends ".."
    // We receive ".." and unstuff to "."
    let lines = ["Content", "..", "."];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 2);
    assert_eq!(result[0], "Content");
    assert_eq!(result[1], "."); // ".." unstuffed to "."
}

#[test]
fn test_multiline_multiple_dot_lines() {
    let lines = ["..", "...", "....", "."];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 3);
    assert_eq!(result[0], ".");
    assert_eq!(result[1], "..");
    assert_eq!(result[2], "...");
}

#[test]
fn test_multiline_xover_line_simulation() {
    // XOVER lines are tab-separated
    let lines = [
        "12345\tSubject\tauthor@example.com\tDate\t<msgid>\trefs\t1000\t50",
        "12346\tAnother\tother@example.com\tDate2\t<msgid2>\trefs2\t2000\t100",
        ".",
    ];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 2);
    assert!(result[0].starts_with("12345\t"));
    assert!(result[1].starts_with("12346\t"));
}

// === RFC Requirements ===

#[test]
fn test_rfc_requirement_no_nul_in_lines() {
    // RFC 3977 §3.1.1: Lines must not contain NUL
    // Our unstuff function handles gracefully
    let line_with_nul = "Hello\0World";
    assert_eq!(unstuff_line(line_with_nul), line_with_nul);
}

#[test]
fn test_rfc_requirement_terminator_not_included_in_data() {
    // RFC 3977 §3.1.1: "do not include the terminating line"
    let lines = ["Data", "."];
    let result = parse_multiline_body(&lines);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0], "Data");
}

// === CRLF Integration Tests ===

#[test]
fn test_crlf_multiline_basic() {
    let raw = "Line 1\r\nLine 2\r\n.\r\n";
    let result = parse_raw_multiline_response(raw);

    assert_eq!(result.len(), 2);
    assert_eq!(result[0], "Line 1");
    assert_eq!(result[1], "Line 2");
}

#[test]
fn test_crlf_multiline_with_dot_stuffing() {
    let raw = "Normal\r\n..Dot-stuffed\r\n.\r\n";
    let result = parse_raw_multiline_response(raw);

    assert_eq!(result.len(), 2);
    assert_eq!(result[0], "Normal");
    assert_eq!(result[1], ".Dot-stuffed");
}

#[test]
fn test_crlf_multiline_empty_body() {
    let raw = ".\r\n";
    let result = parse_raw_multiline_response(raw);
    assert!(result.is_empty());
}

#[test]
fn test_crlf_multiline_article_with_separator() {
    // RFC 3977: blank line between headers and body is significant
    let raw = "From: user@example.com\r\nSubject: Test\r\n\r\nBody text.\r\n.\r\n";
    let lines: Vec<&str> = raw.split("\r\n").collect();

    assert_eq!(lines[0], "From: user@example.com");
    assert_eq!(lines[1], "Subject: Test");
    assert_eq!(lines[2], ""); // Header/body separator
    assert_eq!(lines[3], "Body text.");
    assert_eq!(lines[4], "."); // Terminator
}

#[test]
fn test_crlf_terminator_format() {
    // RFC 3977 §3.1.1: Terminator is ".\r\n"
    let raw = "Content\r\n.\r\n";
    assert!(raw.ends_with(".\r\n"));

    let lines: Vec<&str> = raw.trim_end_matches("\r\n").split("\r\n").collect();
    let last_line = lines.last().unwrap();
    assert_eq!(*last_line, ".");
}

// === Bare CR/LF Handling (Invalid Per RFC) ===

#[test]
fn test_bare_lf_handling() {
    // RFC 3977 requires \r\n; bare \n is not compliant
    let raw = "Line1\nLine2\n.\n";

    // CRLF split treats this as one long line
    let crlf_lines: Vec<&str> = raw.split("\r\n").filter(|s| !s.is_empty()).collect();
    assert_eq!(crlf_lines.len(), 1);

    // LF split would produce multiple lines
    let lf_lines: Vec<&str> = raw.split('\n').filter(|s| !s.is_empty()).collect();
    assert_eq!(lf_lines.len(), 3);
}

#[test]
fn test_bare_cr_handling() {
    // RFC 3977 requires \r\n; bare \r is not compliant
    let raw = "Line1\rLine2\r.\r";

    let crlf_lines: Vec<&str> = raw.split("\r\n").filter(|s| !s.is_empty()).collect();
    assert_eq!(crlf_lines.len(), 1);

    let cr_lines: Vec<&str> = raw.split('\r').filter(|s| !s.is_empty()).collect();
    assert_eq!(cr_lines.len(), 3);
}

#[test]
fn test_mixed_line_endings() {
    // Mixed \r\n and \n is not RFC-compliant
    let raw = "Line1\r\nLine2\nLine3\r\n.\r\n";

    // CRLF parser sees Line2\nLine3 as one line
    let crlf_lines: Vec<&str> = raw.split("\r\n").filter(|s| !s.is_empty()).collect();
    assert!(crlf_lines.iter().any(|l| l.contains("Line2\nLine3")));
}

// === Protocol Constants ===

#[test]
fn test_multiline_terminator_constant() {
    assert_eq!(MULTILINE_TERMINATOR, b"\r\n.\r\n");
    assert_eq!(MULTILINE_TERMINATOR.len(), 5);
}

#[test]
fn test_crlf_constant() {
    assert_eq!(CRLF, b"\r\n");
    assert_eq!(CRLF.len(), 2);
}

// === Terminator Detection (proxy-specific) ===

#[test]
fn test_terminator_in_raw_bytes() {
    let response = b"220 Article follows\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";

    // The response should end with the multiline terminator
    assert!(response.ends_with(MULTILINE_TERMINATOR));
}

#[test]
fn test_no_terminator_in_partial_response() {
    let partial = b"220 Article follows\r\nSubject: Test\r\n";
    assert!(!partial.ends_with(MULTILINE_TERMINATOR));
}

#[test]
fn test_terminator_detection_with_dot_content() {
    // A line that's ".." followed by terminator should not confuse detection
    let response = b"222 Body\r\n..Content starting with dot\r\n.\r\n";
    assert!(response.ends_with(MULTILINE_TERMINATOR));

    // Verify the ".." is content, not terminator
    let body_start = b"222 Body\r\n".len();
    let first_content_line = &response[body_start..body_start + 2];
    assert_eq!(first_content_line, b"..");
}
