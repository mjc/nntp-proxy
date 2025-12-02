//! Tests for input validation and error handling

use crate::protocol::response::*;

#[test]
fn test_greeting_variations() {
    // Standard greeting
    assert!(matches!(
        NntpResponse::parse(b"200 news.example.com ready\r\n"),
        NntpResponse::Greeting(_)
    ));

    // Read-only greeting
    assert!(matches!(
        NntpResponse::parse(b"201 read-only access\r\n"),
        NntpResponse::Greeting(_)
    ));

    // Minimal greeting
    assert!(matches!(
        NntpResponse::parse(b"200\r\n"),
        NntpResponse::Greeting(_)
    ));

    // Greeting without CRLF
    assert!(matches!(
        NntpResponse::parse(b"200 Ready"),
        NntpResponse::Greeting(_)
    ));

    // Not a greeting (4xx/5xx codes)
    assert!(!matches!(
        NntpResponse::parse(b"400 Service unavailable\r\n"),
        NntpResponse::Greeting(_)
    ));
    assert!(!matches!(
        NntpResponse::parse(b"502 Service unavailable\r\n"),
        NntpResponse::Greeting(_)
    ));

    // Auth-related but not greeting
    assert!(!matches!(
        NntpResponse::parse(b"281 Auth accepted\r\n"),
        NntpResponse::Greeting(_)
    ));
    assert!(!matches!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::Greeting(_)
    ));
}

#[test]
fn test_auth_required_edge_cases() {
    // Standard auth required
    assert!(matches!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::AuthRequired(_)
    ));
    assert!(matches!(
        NntpResponse::parse(b"480 Authentication required\r\n"),
        NntpResponse::AuthRequired(_)
    ));

    // Without message
    assert!(matches!(
        NntpResponse::parse(b"381\r\n"),
        NntpResponse::AuthRequired(_)
    ));
    assert!(matches!(
        NntpResponse::parse(b"480\r\n"),
        NntpResponse::AuthRequired(_)
    ));

    // Without CRLF
    assert!(matches!(
        NntpResponse::parse(b"381 Password required"),
        NntpResponse::AuthRequired(_)
    ));

    // Not auth required
    assert!(!matches!(
        NntpResponse::parse(b"200 Ready\r\n"),
        NntpResponse::AuthRequired(_)
    ));
    assert!(!matches!(
        NntpResponse::parse(b"281 Auth accepted\r\n"),
        NntpResponse::AuthRequired(_)
    ));
    assert!(!matches!(
        NntpResponse::parse(b"482 Auth rejected\r\n"),
        NntpResponse::AuthRequired(_)
    ));
}

#[test]
fn test_auth_success_edge_cases() {
    // Standard success
    assert_eq!(
        NntpResponse::parse(b"281 Authentication accepted\r\n"),
        NntpResponse::AuthSuccess
    );

    // Minimal success
    assert_eq!(NntpResponse::parse(b"281\r\n"), NntpResponse::AuthSuccess);

    // Without CRLF
    assert_eq!(NntpResponse::parse(b"281 OK"), NntpResponse::AuthSuccess);

    // Not success (other 2xx codes)
    assert_ne!(
        NntpResponse::parse(b"200 Ready\r\n"),
        NntpResponse::AuthSuccess
    );
    assert_ne!(
        NntpResponse::parse(b"211 Group selected\r\n"),
        NntpResponse::AuthSuccess
    );

    // Failures
    assert_ne!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::AuthSuccess
    );
    assert_ne!(
        NntpResponse::parse(b"481 Authentication failed\r\n"),
        NntpResponse::AuthSuccess
    );
    assert_ne!(
        NntpResponse::parse(b"482 Authentication rejected\r\n"),
        NntpResponse::AuthSuccess
    );
}

#[test]
fn test_success_response_edge_cases() {
    // 2xx codes (success)
    assert!(NntpResponse::parse(b"200 OK\r\n").is_success());
    assert!(NntpResponse::parse(b"211 Group selected\r\n").is_success());
    assert!(NntpResponse::parse(b"220 Article follows\r\n").is_success());
    assert!(NntpResponse::parse(b"281 Auth OK\r\n").is_success());

    // 1xx codes (informational - not success)
    assert!(!NntpResponse::parse(b"100 Info\r\n").is_success());

    // 3xx codes (further action - still considered success in NNTP context)
    assert!(NntpResponse::parse(b"381 Password required\r\n").is_success());

    // 4xx codes (client error)
    assert!(!NntpResponse::parse(b"400 Bad request\r\n").is_success());
    assert!(!NntpResponse::parse(b"430 No such article\r\n").is_success());

    // 5xx codes (server error)
    assert!(!NntpResponse::parse(b"500 Internal error\r\n").is_success());
    assert!(!NntpResponse::parse(b"502 Service unavailable\r\n").is_success());
}
