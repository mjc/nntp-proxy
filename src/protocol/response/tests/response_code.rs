//! Tests for Response enum

use crate::protocol::response::*;

#[test]
fn test_response_code_parse() {
    // Greetings
    assert_eq!(
        NntpResponse::parse(b"200 Ready\r\n"),
        NntpResponse::Greeting(StatusCode::new(200))
    );
    assert_eq!(
        NntpResponse::parse(b"201 No posting\r\n"),
        NntpResponse::Greeting(StatusCode::new(201))
    );

    // Disconnect
    assert_eq!(
        NntpResponse::parse(b"205 Goodbye\r\n"),
        NntpResponse::Disconnect
    );

    // Auth
    assert_eq!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::AuthRequired(StatusCode::new(381))
    );
    assert_eq!(
        NntpResponse::parse(b"480 Auth required\r\n"),
        NntpResponse::AuthRequired(StatusCode::new(480))
    );
    assert_eq!(
        NntpResponse::parse(b"281 Auth success\r\n"),
        NntpResponse::AuthSuccess
    );

    // Multiline
    assert_eq!(
        NntpResponse::parse(b"100 Help\r\n"),
        NntpResponse::MultilineData(StatusCode::new(100))
    );
    assert_eq!(
        NntpResponse::parse(b"215 LIST\r\n"),
        NntpResponse::MultilineData(StatusCode::new(215))
    );
    assert_eq!(
        NntpResponse::parse(b"220 Article\r\n"),
        NntpResponse::MultilineData(StatusCode::new(220))
    );

    // Single-line
    assert_eq!(
        NntpResponse::parse(b"211 Group selected\r\n"),
        NntpResponse::SingleLine(StatusCode::new(211))
    );
    assert_eq!(
        NntpResponse::parse(b"400 Error\r\n"),
        NntpResponse::SingleLine(StatusCode::new(400))
    );

    // Invalid
    assert_eq!(NntpResponse::parse(b"XXX\r\n"), NntpResponse::Invalid);
}

#[test]
fn test_response_code_is_multiline() {
    assert!(NntpResponse::parse(b"215 LIST\r\n").is_multiline());
    assert!(NntpResponse::parse(b"220 Article\r\n").is_multiline());
    assert!(!NntpResponse::parse(b"200 Ready\r\n").is_multiline());
    assert!(!NntpResponse::parse(b"211 Group\r\n").is_multiline());
}

#[test]
fn test_response_code_status_code() {
    assert_eq!(
        NntpResponse::parse(b"200 OK\r\n").status_code(),
        Some(StatusCode::new(200))
    );
    assert_eq!(
        NntpResponse::Disconnect.status_code(),
        Some(StatusCode::new(205))
    );
    assert_eq!(
        NntpResponse::AuthSuccess.status_code(),
        Some(StatusCode::new(281))
    );
    assert_eq!(NntpResponse::Invalid.status_code(), None);
}

#[test]
fn test_response_code_is_success() {
    assert!(NntpResponse::parse(b"200 OK\r\n").is_success());
    assert!(NntpResponse::parse(b"215 LIST\r\n").is_success());
    assert!(NntpResponse::parse(b"381 Auth\r\n").is_success());
    assert!(!NntpResponse::parse(b"400 Error\r\n").is_success());
    assert!(!NntpResponse::Invalid.is_success());
}
