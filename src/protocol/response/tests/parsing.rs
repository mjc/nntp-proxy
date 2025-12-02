//! Tests for response parsing functions

use crate::protocol::response::*;

#[test]
fn test_parse_status_code() {
    assert_eq!(
        StatusCode::parse(b"200 Ready\r\n"),
        Some(StatusCode::new(200))
    );
    assert_eq!(
        StatusCode::parse(b"381 Password required\r\n"),
        Some(StatusCode::new(381))
    );
    assert_eq!(
        StatusCode::parse(b"500 Error\r\n"),
        Some(StatusCode::new(500))
    );
    assert_eq!(StatusCode::parse(b"XX"), None);
    assert_eq!(StatusCode::parse(b""), None);
}

#[test]
fn test_is_success_response() {
    assert!(NntpResponse::parse(b"200 Ready\r\n").is_success());
    assert!(NntpResponse::parse(b"281 Auth OK\r\n").is_success());
    assert!(!NntpResponse::parse(b"400 Error\r\n").is_success());
    assert!(!NntpResponse::parse(b"500 Error\r\n").is_success());
}

#[test]
fn test_is_greeting() {
    assert!(matches!(
        NntpResponse::parse(b"200 news.example.com ready\r\n"),
        NntpResponse::Greeting(_)
    ));
    assert!(matches!(
        NntpResponse::parse(b"201 read-only\r\n"),
        NntpResponse::Greeting(_)
    ));
    assert!(!matches!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::Greeting(_)
    ));
}

#[test]
fn test_auth_responses() {
    assert!(matches!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::AuthRequired(_)
    ));
    assert!(matches!(
        NntpResponse::parse(b"480 Auth required\r\n"),
        NntpResponse::AuthRequired(_)
    ));
    assert!(!matches!(
        NntpResponse::parse(b"200 Ready\r\n"),
        NntpResponse::AuthRequired(_)
    ));

    assert_eq!(
        NntpResponse::parse(b"281 Auth accepted\r\n"),
        NntpResponse::AuthSuccess
    );
    assert_ne!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::AuthSuccess
    );
}

#[test]
fn test_is_response_code() {
    assert_eq!(
        StatusCode::parse(b"111 20251010120000\r\n").map(|c| c.as_u16()),
        Some(111)
    );
    assert_eq!(
        StatusCode::parse(b"200 Ready\r\n").map(|c| c.as_u16()),
        Some(200)
    );
    assert_eq!(
        StatusCode::parse(b"201 Read-only\r\n").map(|c| c.as_u16()),
        Some(201)
    );
    assert_eq!(
        StatusCode::parse(b"281 Auth OK\r\n").map(|c| c.as_u16()),
        Some(281)
    );
    assert_eq!(
        StatusCode::parse(b"381 Password required\r\n").map(|c| c.as_u16()),
        Some(381)
    );
    assert_ne!(
        StatusCode::parse(b"200 Ready\r\n").map(|c| c.as_u16()),
        Some(201)
    );
    assert_ne!(
        StatusCode::parse(b"111 Date\r\n").map(|c| c.as_u16()),
        Some(200)
    );
}

#[test]
fn test_malformed_status_codes() {
    // Non-numeric status
    assert_eq!(StatusCode::parse(b"ABC Invalid\r\n"), None);

    // Missing status code
    assert_eq!(StatusCode::parse(b"Missing code\r\n"), None);

    // Incomplete status code
    assert_eq!(StatusCode::parse(b"20"), None);
    assert_eq!(StatusCode::parse(b"2"), None);

    // Status code with invalid characters
    assert_eq!(StatusCode::parse(b"2X0 Error\r\n"), None);

    // Negative status (shouldn't parse)
    assert_eq!(StatusCode::parse(b"-200 Invalid\r\n"), None);
}

#[test]
fn test_incomplete_responses() {
    // Empty response
    assert_eq!(StatusCode::parse(b""), None);

    // Only newline
    assert_eq!(StatusCode::parse(b"\r\n"), None);

    // Status without message
    assert_eq!(StatusCode::parse(b"200"), Some(StatusCode::new(200)));

    // Status with just space
    assert_eq!(StatusCode::parse(b"200 "), Some(StatusCode::new(200)));
}

#[test]
fn test_boundary_status_codes() {
    // Minimum valid code
    assert_eq!(
        StatusCode::parse(b"100 Info\r\n"),
        Some(StatusCode::new(100))
    );

    // Maximum valid code
    assert_eq!(
        StatusCode::parse(b"599 Error\r\n"),
        Some(StatusCode::new(599))
    );

    // Out of range codes (but still parse)
    assert_eq!(StatusCode::parse(b"000 Zero\r\n"), Some(StatusCode::new(0)));
    assert_eq!(
        StatusCode::parse(b"999 Max\r\n"),
        Some(StatusCode::new(999))
    );

    // Four digit code (only first 3 parsed)
    assert_eq!(
        StatusCode::parse(b"1234 Invalid\r\n"),
        Some(StatusCode::new(123))
    );
}
