//! Tests for response parsing functions

use crate::protocol::response::*;

#[test]
fn test_parse_status_code() {
    assert_eq!(
        NntpResponse::parse_status_code(b"200 Ready\r\n"),
        Some(StatusCode::new(200))
    );
    assert_eq!(
        NntpResponse::parse_status_code(b"381 Password required\r\n"),
        Some(StatusCode::new(381))
    );
    assert_eq!(
        NntpResponse::parse_status_code(b"500 Error\r\n"),
        Some(StatusCode::new(500))
    );
    assert_eq!(NntpResponse::parse_status_code(b"XX"), None);
    assert_eq!(NntpResponse::parse_status_code(b""), None);
}

#[test]
fn test_is_success_response() {
    assert!(ResponseParser::is_success_response(b"200 Ready\r\n"));
    assert!(ResponseParser::is_success_response(b"281 Auth OK\r\n"));
    assert!(!ResponseParser::is_success_response(b"400 Error\r\n"));
    assert!(!ResponseParser::is_success_response(b"500 Error\r\n"));
}

#[test]
fn test_is_greeting() {
    assert!(ResponseParser::is_greeting(
        b"200 news.example.com ready\r\n"
    ));
    assert!(ResponseParser::is_greeting(b"201 read-only\r\n"));
    assert!(!ResponseParser::is_greeting(b"381 Password required\r\n"));
}

#[test]
fn test_auth_responses() {
    assert!(ResponseParser::is_auth_required(
        b"381 Password required\r\n"
    ));
    assert!(ResponseParser::is_auth_required(b"480 Auth required\r\n"));
    assert!(!ResponseParser::is_auth_required(b"200 Ready\r\n"));

    assert!(ResponseParser::is_auth_success(b"281 Auth accepted\r\n"));
    assert!(!ResponseParser::is_auth_success(
        b"381 Password required\r\n"
    ));
}

#[test]
fn test_is_response_code() {
    assert!(ResponseParser::is_response_code(
        b"111 20251010120000\r\n",
        111
    ));
    assert!(ResponseParser::is_response_code(b"200 Ready\r\n", 200));
    assert!(ResponseParser::is_response_code(b"201 Read-only\r\n", 201));
    assert!(ResponseParser::is_response_code(b"281 Auth OK\r\n", 281));
    assert!(ResponseParser::is_response_code(
        b"381 Password required\r\n",
        381
    ));
    assert!(!ResponseParser::is_response_code(b"200 Ready\r\n", 201));
    assert!(!ResponseParser::is_response_code(b"111 Date\r\n", 200));
}

#[test]
fn test_malformed_status_codes() {
    // Non-numeric status
    assert_eq!(NntpResponse::parse_status_code(b"ABC Invalid\r\n"), None);

    // Missing status code
    assert_eq!(NntpResponse::parse_status_code(b"Missing code\r\n"), None);

    // Incomplete status code
    assert_eq!(NntpResponse::parse_status_code(b"20"), None);
    assert_eq!(NntpResponse::parse_status_code(b"2"), None);

    // Status code with invalid characters
    assert_eq!(NntpResponse::parse_status_code(b"2X0 Error\r\n"), None);

    // Negative status (shouldn't parse)
    assert_eq!(NntpResponse::parse_status_code(b"-200 Invalid\r\n"), None);
}

#[test]
fn test_incomplete_responses() {
    // Empty response
    assert_eq!(NntpResponse::parse_status_code(b""), None);

    // Only newline
    assert_eq!(NntpResponse::parse_status_code(b"\r\n"), None);

    // Status without message
    assert_eq!(
        NntpResponse::parse_status_code(b"200"),
        Some(StatusCode::new(200))
    );

    // Status with just space
    assert_eq!(
        NntpResponse::parse_status_code(b"200 "),
        Some(StatusCode::new(200))
    );
}

#[test]
fn test_boundary_status_codes() {
    // Minimum valid code
    assert_eq!(
        NntpResponse::parse_status_code(b"100 Info\r\n"),
        Some(StatusCode::new(100))
    );

    // Maximum valid code
    assert_eq!(
        NntpResponse::parse_status_code(b"599 Error\r\n"),
        Some(StatusCode::new(599))
    );

    // Out of range codes (but still parse)
    assert_eq!(
        NntpResponse::parse_status_code(b"000 Zero\r\n"),
        Some(StatusCode::new(0))
    );
    assert_eq!(
        NntpResponse::parse_status_code(b"999 Max\r\n"),
        Some(StatusCode::new(999))
    );

    // Four digit code (only first 3 parsed)
    assert_eq!(
        NntpResponse::parse_status_code(b"1234 Invalid\r\n"),
        Some(StatusCode::new(123))
    );
}
