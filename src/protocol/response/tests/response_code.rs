//! Tests for ResponseCode enum

use crate::protocol::response::*;

#[test]
fn test_response_code_parse() {
    // Greetings
    assert_eq!(
        ResponseCode::parse(b"200 Ready\r\n"),
        ResponseCode::Greeting(200)
    );
    assert_eq!(
        ResponseCode::parse(b"201 No posting\r\n"),
        ResponseCode::Greeting(201)
    );

    // Disconnect
    assert_eq!(
        ResponseCode::parse(b"205 Goodbye\r\n"),
        ResponseCode::Disconnect
    );

    // Auth
    assert_eq!(
        ResponseCode::parse(b"381 Password required\r\n"),
        ResponseCode::AuthRequired(381)
    );
    assert_eq!(
        ResponseCode::parse(b"480 Auth required\r\n"),
        ResponseCode::AuthRequired(480)
    );
    assert_eq!(
        ResponseCode::parse(b"281 Auth success\r\n"),
        ResponseCode::AuthSuccess
    );

    // Multiline
    assert_eq!(
        ResponseCode::parse(b"100 Help\r\n"),
        ResponseCode::MultilineData(100)
    );
    assert_eq!(
        ResponseCode::parse(b"215 LIST\r\n"),
        ResponseCode::MultilineData(215)
    );
    assert_eq!(
        ResponseCode::parse(b"220 Article\r\n"),
        ResponseCode::MultilineData(220)
    );

    // Single-line
    assert_eq!(
        ResponseCode::parse(b"211 Group selected\r\n"),
        ResponseCode::SingleLine(211)
    );
    assert_eq!(
        ResponseCode::parse(b"400 Error\r\n"),
        ResponseCode::SingleLine(400)
    );

    // Invalid
    assert_eq!(ResponseCode::parse(b"XXX\r\n"), ResponseCode::Invalid);
}

#[test]
fn test_response_code_is_multiline() {
    assert!(ResponseCode::parse(b"215 LIST\r\n").is_multiline());
    assert!(ResponseCode::parse(b"220 Article\r\n").is_multiline());
    assert!(!ResponseCode::parse(b"200 Ready\r\n").is_multiline());
    assert!(!ResponseCode::parse(b"211 Group\r\n").is_multiline());
}

#[test]
fn test_response_code_status_code() {
    assert_eq!(ResponseCode::parse(b"200 OK\r\n").status_code(), Some(200));
    assert_eq!(ResponseCode::Disconnect.status_code(), Some(205));
    assert_eq!(ResponseCode::AuthSuccess.status_code(), Some(281));
    assert_eq!(ResponseCode::Invalid.status_code(), None);
}

#[test]
fn test_response_code_is_success() {
    assert!(ResponseCode::parse(b"200 OK\r\n").is_success());
    assert!(ResponseCode::parse(b"215 LIST\r\n").is_success());
    assert!(ResponseCode::parse(b"381 Auth\r\n").is_success());
    assert!(!ResponseCode::parse(b"400 Error\r\n").is_success());
    assert!(!ResponseCode::Invalid.is_success());
}
