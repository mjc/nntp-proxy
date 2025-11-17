//! Tests for Response enum

use crate::protocol::response::*;

#[test]
fn test_response_code_parse() {
    // Greetings
    assert_eq!(
        Response::parse(b"200 Ready\r\n"),
        Response::Greeting(StatusCode::new(200))
    );
    assert_eq!(
        Response::parse(b"201 No posting\r\n"),
        Response::Greeting(StatusCode::new(201))
    );

    // Disconnect
    assert_eq!(Response::parse(b"205 Goodbye\r\n"), Response::Disconnect);

    // Auth
    assert_eq!(
        Response::parse(b"381 Password required\r\n"),
        Response::AuthRequired(StatusCode::new(381))
    );
    assert_eq!(
        Response::parse(b"480 Auth required\r\n"),
        Response::AuthRequired(StatusCode::new(480))
    );
    assert_eq!(
        Response::parse(b"281 Auth success\r\n"),
        Response::AuthSuccess
    );

    // Multiline
    assert_eq!(
        Response::parse(b"100 Help\r\n"),
        Response::MultilineData(StatusCode::new(100))
    );
    assert_eq!(
        Response::parse(b"215 LIST\r\n"),
        Response::MultilineData(StatusCode::new(215))
    );
    assert_eq!(
        Response::parse(b"220 Article\r\n"),
        Response::MultilineData(StatusCode::new(220))
    );

    // Single-line
    assert_eq!(
        Response::parse(b"211 Group selected\r\n"),
        Response::SingleLine(StatusCode::new(211))
    );
    assert_eq!(
        Response::parse(b"400 Error\r\n"),
        Response::SingleLine(StatusCode::new(400))
    );

    // Invalid
    assert_eq!(Response::parse(b"XXX\r\n"), Response::Invalid);
}

#[test]
fn test_response_code_is_multiline() {
    assert!(Response::parse(b"215 LIST\r\n").is_multiline());
    assert!(Response::parse(b"220 Article\r\n").is_multiline());
    assert!(!Response::parse(b"200 Ready\r\n").is_multiline());
    assert!(!Response::parse(b"211 Group\r\n").is_multiline());
}

#[test]
fn test_response_code_status_code() {
    assert_eq!(
        Response::parse(b"200 OK\r\n").status_code(),
        Some(StatusCode::new(200))
    );
    assert_eq!(
        Response::Disconnect.status_code(),
        Some(StatusCode::new(205))
    );
    assert_eq!(
        Response::AuthSuccess.status_code(),
        Some(StatusCode::new(281))
    );
    assert_eq!(Response::Invalid.status_code(), None);
}

#[test]
fn test_response_code_is_success() {
    assert!(Response::parse(b"200 OK\r\n").is_success());
    assert!(Response::parse(b"215 LIST\r\n").is_success());
    assert!(Response::parse(b"381 Auth\r\n").is_success());
    assert!(!Response::parse(b"400 Error\r\n").is_success());
    assert!(!Response::Invalid.is_success());
}
