//! Tests for input validation and error handling

use crate::protocol::response::*;

#[test]
fn test_greeting_variations() {
    // Standard greeting
    assert!(ResponseParser::is_greeting(
        b"200 news.example.com ready\r\n"
    ));

    // Read-only greeting
    assert!(ResponseParser::is_greeting(b"201 read-only access\r\n"));

    // Minimal greeting
    assert!(ResponseParser::is_greeting(b"200\r\n"));

    // Greeting without CRLF
    assert!(ResponseParser::is_greeting(b"200 Ready"));

    // Not a greeting (4xx/5xx codes)
    assert!(!ResponseParser::is_greeting(b"400 Service unavailable\r\n"));
    assert!(!ResponseParser::is_greeting(b"502 Service unavailable\r\n"));

    // Auth-related but not greeting
    assert!(!ResponseParser::is_greeting(b"281 Auth accepted\r\n"));
    assert!(!ResponseParser::is_greeting(b"381 Password required\r\n"));
}

#[test]
fn test_auth_required_edge_cases() {
    // Standard auth required
    assert!(ResponseParser::is_auth_required(
        b"381 Password required\r\n"
    ));
    assert!(ResponseParser::is_auth_required(
        b"480 Authentication required\r\n"
    ));

    // Without message
    assert!(ResponseParser::is_auth_required(b"381\r\n"));
    assert!(ResponseParser::is_auth_required(b"480\r\n"));

    // Without CRLF
    assert!(ResponseParser::is_auth_required(b"381 Password required"));

    // Not auth required
    assert!(!ResponseParser::is_auth_required(b"200 Ready\r\n"));
    assert!(!ResponseParser::is_auth_required(b"281 Auth accepted\r\n"));
    assert!(!ResponseParser::is_auth_required(b"482 Auth rejected\r\n"));
}

#[test]
fn test_auth_success_edge_cases() {
    // Standard success
    assert!(ResponseParser::is_auth_success(
        b"281 Authentication accepted\r\n"
    ));

    // Minimal success
    assert!(ResponseParser::is_auth_success(b"281\r\n"));

    // Without CRLF
    assert!(ResponseParser::is_auth_success(b"281 OK"));

    // Not success (other 2xx codes)
    assert!(!ResponseParser::is_auth_success(b"200 Ready\r\n"));
    assert!(!ResponseParser::is_auth_success(b"211 Group selected\r\n"));

    // Failures
    assert!(!ResponseParser::is_auth_success(
        b"381 Password required\r\n"
    ));
    assert!(!ResponseParser::is_auth_success(
        b"481 Authentication failed\r\n"
    ));
    assert!(!ResponseParser::is_auth_success(
        b"482 Authentication rejected\r\n"
    ));
}

#[test]
fn test_success_response_edge_cases() {
    // 2xx codes (success)
    assert!(ResponseParser::is_success_response(b"200 OK\r\n"));
    assert!(ResponseParser::is_success_response(
        b"211 Group selected\r\n"
    ));
    assert!(ResponseParser::is_success_response(
        b"220 Article follows\r\n"
    ));
    assert!(ResponseParser::is_success_response(b"281 Auth OK\r\n"));

    // 1xx codes (informational - not success)
    assert!(!ResponseParser::is_success_response(b"100 Info\r\n"));

    // 3xx codes (further action - still considered success in NNTP context)
    assert!(ResponseParser::is_success_response(
        b"381 Password required\r\n"
    ));

    // 4xx codes (client error)
    assert!(!ResponseParser::is_success_response(b"400 Bad request\r\n"));
    assert!(!ResponseParser::is_success_response(
        b"430 No such article\r\n"
    ));

    // 5xx codes (server error)
    assert!(!ResponseParser::is_success_response(
        b"500 Internal error\r\n"
    ));
    assert!(!ResponseParser::is_success_response(
        b"502 Service unavailable\r\n"
    ));
}
