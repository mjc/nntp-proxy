//! Edge case tests for unusual inputs

use crate::protocol::response::*;

#[test]
fn test_utf8_in_responses() {
    // Response with UTF-8 characters in message
    let utf8_response = "200 Привет мир\r\n".as_bytes();
    assert_eq!(NntpResponse::parse_status_code(utf8_response), Some(200));
    assert!(ResponseParser::is_greeting(utf8_response));

    // Auth response with UTF-8
    let utf8_auth = "281 认证成功\r\n".as_bytes();
    assert_eq!(NntpResponse::parse_status_code(utf8_auth), Some(281));
    assert!(ResponseParser::is_auth_success(utf8_auth));
}

#[test]
fn test_response_with_binary_data() {
    // Response with null bytes in message part (after status code)
    let with_null = b"200 Test\x00Message\r\n";
    assert_eq!(NntpResponse::parse_status_code(with_null), Some(200));

    // Response with high bytes
    let with_high_bytes = b"200 \xFF\xFE\r\n";
    assert_eq!(NntpResponse::parse_status_code(with_high_bytes), Some(200));
}
