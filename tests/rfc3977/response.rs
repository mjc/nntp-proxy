//! RFC 3977 Section 3.2 - Response Code Classification Tests
//!
//! These tests verify correct classification of NNTP response codes:
//! - 1xx: Informational
//! - 2xx: Success
//! - 3xx: Continuation (send more input)
//! - 4xx: Temporary error
//! - 5xx: Permanent error
//!
//! Adapted from nntp-rs RFC 3977 response.rs and parsing.rs tests.

use nntp_proxy::protocol::{NntpResponse, StatusCode};

// StatusCode classification

#[test]
fn test_response_is_success_2xx() {
    let code = StatusCode::new(200);
    assert!(code.is_success());
    assert!(!code.is_error());
    assert!(!code.is_informational());
    assert!(!code.is_continuation());
}

#[test]
fn test_response_is_success_3xx() {
    // Per RFC 3977 §3.2.1: 3xx is "success so far"
    let code = StatusCode::new(381);
    assert!(code.is_success());
    assert!(code.is_continuation());
    assert!(!code.is_error());
}

#[test]
fn test_response_is_continuation() {
    let code = StatusCode::new(335);
    assert!(code.is_continuation());
    assert!(code.is_success()); // 3xx counts as success
}

#[test]
fn test_response_is_error_4xx() {
    let code = StatusCode::new(430);
    assert!(code.is_error());
    assert!(!code.is_success());
    assert!(!code.is_continuation());
}

#[test]
fn test_response_is_error_5xx() {
    let code = StatusCode::new(500);
    assert!(code.is_error());
    assert!(!code.is_success());
}

#[test]
fn test_response_1xx_informational() {
    let code = StatusCode::new(100);
    assert!(code.is_informational());
    assert!(!code.is_success());
    assert!(!code.is_error());
    assert!(!code.is_continuation());
}

// Boundary code tests - verify exact transitions between categories

#[test]
fn test_boundary_between_informational_and_success() {
    let code_199 = StatusCode::new(199);
    assert!(code_199.is_informational());
    assert!(!code_199.is_success());

    let code_200 = StatusCode::new(200);
    assert!(!code_200.is_informational());
    assert!(code_200.is_success());
}

#[test]
fn test_boundary_between_success_and_continuation() {
    let code_299 = StatusCode::new(299);
    assert!(code_299.is_success());
    assert!(!code_299.is_continuation());

    let code_300 = StatusCode::new(300);
    assert!(code_300.is_success()); // 3xx is also success
    assert!(code_300.is_continuation());
}

#[test]
fn test_boundary_between_continuation_and_error() {
    let code_399 = StatusCode::new(399);
    assert!(code_399.is_continuation());
    assert!(code_399.is_success());
    assert!(!code_399.is_error());

    let code_400 = StatusCode::new(400);
    assert!(!code_400.is_continuation());
    assert!(!code_400.is_success());
    assert!(code_400.is_error());
}

#[test]
fn test_boundary_between_4xx_and_5xx() {
    let code_499 = StatusCode::new(499);
    assert!(code_499.is_error());

    let code_500 = StatusCode::new(500);
    assert!(code_500.is_error());
}

// All 2xx success codes

#[test]
fn test_all_2xx_success_codes() {
    let codes = [
        200, 201, 205, 211, 215, 220, 221, 222, 223, 224, 225, 230, 231, 281, 282,
    ];
    for code in codes {
        let sc = StatusCode::new(code);
        assert!(sc.is_success(), "Code {} should be success", code);
        assert!(!sc.is_error(), "Code {} should not be error", code);
    }
}

// All 3xx continuation codes

#[test]
fn test_all_3xx_continuation_codes() {
    let codes = [335, 340, 381, 383];
    for code in codes {
        let sc = StatusCode::new(code);
        assert!(sc.is_continuation(), "Code {} should be continuation", code);
        assert!(sc.is_success(), "Code {} should also be success", code);
    }
}

// Specific 3xx codes

#[test]
fn test_response_335_ihave_send_article() {
    let code = StatusCode::new(335);
    assert!(code.is_continuation());
}

#[test]
fn test_response_340_post_send_article() {
    let code = StatusCode::new(340);
    assert!(code.is_continuation());
}

#[test]
fn test_response_381_password_required() {
    let code = StatusCode::new(381);
    assert!(code.is_continuation());
}

#[test]
fn test_response_383_sasl_continue() {
    let code = StatusCode::new(383);
    assert!(code.is_continuation());
}

// 1xx informational codes

#[test]
fn test_response_100_help_text() {
    let code = StatusCode::new(100);
    assert!(code.is_informational());
    assert!(code.is_multiline());
}

#[test]
fn test_response_101_capability_list() {
    let code = StatusCode::new(101);
    assert!(code.is_informational());
    assert!(code.is_multiline());
}

#[test]
fn test_response_111_server_date() {
    let code = StatusCode::new(111);
    assert!(code.is_informational());
    assert!(code.is_multiline());
}

// StatusCode::parse() tests

#[test]
fn test_parse_three_digit_code_with_message() {
    let parsed = StatusCode::parse(b"200 Service ready\r\n");
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

#[test]
fn test_parse_three_digit_code_only() {
    let parsed = StatusCode::parse(b"200");
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

#[test]
fn test_parse_with_empty_message_after_space() {
    let parsed = StatusCode::parse(b"200 ");
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

#[test]
fn test_parse_empty_string_is_invalid() {
    assert_eq!(StatusCode::parse(b""), None);
}

#[test]
fn test_parse_two_digit_code_is_invalid() {
    assert_eq!(StatusCode::parse(b"20"), None);
}

#[test]
fn test_parse_one_digit_code_is_invalid() {
    assert_eq!(StatusCode::parse(b"2"), None);
}

#[test]
fn test_parse_non_digit_in_code_is_invalid() {
    assert_eq!(StatusCode::parse(b"2X0 Error\r\n"), None);
    assert_eq!(StatusCode::parse(b"ABC Invalid\r\n"), None);
}

#[test]
fn test_parse_space_prefix_is_invalid() {
    // Space at position 0 is not a digit
    assert_eq!(StatusCode::parse(b" 200 Error\r\n"), None);
}

#[test]
fn test_parse_code_000() {
    // 000 is technically parseable (3 digits)
    let parsed = StatusCode::parse(b"000");
    assert_eq!(parsed, Some(StatusCode::new(0)));
}

#[test]
fn test_parse_code_999() {
    let parsed = StatusCode::parse(b"999");
    assert_eq!(parsed, Some(StatusCode::new(999)));
}

#[test]
fn test_parse_preserves_message_content() {
    // Parsing extracts the code; message content doesn't affect code
    let parsed = StatusCode::parse(b"211 42 1 100 alt.test");
    assert_eq!(parsed, Some(StatusCode::new(211)));
}

#[test]
fn test_parse_message_with_special_chars() {
    let parsed = StatusCode::parse(b"200 Welcome! <server@example>\r\n");
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

#[test]
fn test_parse_long_message() {
    let long_msg = format!("200 {}\r\n", "x".repeat(1000));
    let parsed = StatusCode::parse(long_msg.as_bytes());
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

#[test]
fn test_parse_unicode_message() {
    let parsed = StatusCode::parse("200 Привет мир\r\n".as_bytes());
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

#[test]
fn test_parse_multiple_spaces_in_message() {
    let parsed = StatusCode::parse(b"200  multiple  spaces  \r\n");
    assert_eq!(parsed, Some(StatusCode::new(200)));
}

// NntpResponse categorization

#[test]
fn test_response_greeting_200() {
    assert!(matches!(
        NntpResponse::parse(b"200 OK\r\n"),
        NntpResponse::Greeting(_)
    ));
}

#[test]
fn test_response_greeting_201() {
    assert!(matches!(
        NntpResponse::parse(b"201 No posting\r\n"),
        NntpResponse::Greeting(_)
    ));
}

#[test]
fn test_response_disconnect_205() {
    assert_eq!(
        NntpResponse::parse(b"205 Bye\r\n"),
        NntpResponse::Disconnect
    );
}

#[test]
fn test_response_auth_success_281() {
    assert_eq!(
        NntpResponse::parse(b"281 OK\r\n"),
        NntpResponse::AuthSuccess
    );
}

#[test]
fn test_response_auth_required_381() {
    assert!(matches!(
        NntpResponse::parse(b"381 Password required\r\n"),
        NntpResponse::AuthRequired(_)
    ));
}

#[test]
fn test_response_auth_required_480() {
    assert!(matches!(
        NntpResponse::parse(b"480 Auth required\r\n"),
        NntpResponse::AuthRequired(_)
    ));
}

#[test]
fn test_response_multiline_1xx() {
    assert!(NntpResponse::parse(b"100 Help\r\n").is_multiline());
    assert!(NntpResponse::parse(b"101 Capabilities\r\n").is_multiline());
}

#[test]
fn test_response_multiline_specific_2xx() {
    let multiline_codes = [215, 220, 221, 222, 224, 225, 230, 231, 282];
    for code in multiline_codes {
        let data = format!("{} Test\r\n", code);
        assert!(
            NntpResponse::parse(data.as_bytes()).is_multiline(),
            "Code {} should be multiline",
            code
        );
    }
}

#[test]
fn test_response_non_multiline_2xx() {
    // 200, 201, 205, 211 are NOT multiline
    let non_multiline = [200, 201, 205, 211, 281];
    for code in non_multiline {
        let data = format!("{} Test\r\n", code);
        assert!(
            !NntpResponse::parse(data.as_bytes()).is_multiline(),
            "Code {} should not be multiline",
            code
        );
    }
}

#[test]
fn test_response_single_line_4xx() {
    assert!(matches!(
        NntpResponse::parse(b"400 Error\r\n"),
        NntpResponse::SingleLine(_)
    ));
    assert!(matches!(
        NntpResponse::parse(b"430 No such article\r\n"),
        NntpResponse::SingleLine(_)
    ));
}

#[test]
fn test_response_single_line_5xx() {
    assert!(matches!(
        NntpResponse::parse(b"500 Not recognized\r\n"),
        NntpResponse::SingleLine(_)
    ));
}

#[test]
fn test_response_invalid_empty() {
    assert_eq!(NntpResponse::parse(b""), NntpResponse::Invalid);
}

#[test]
fn test_response_invalid_non_digit() {
    assert_eq!(NntpResponse::parse(b"XXX\r\n"), NntpResponse::Invalid);
}

#[test]
fn test_response_is_success_method() {
    assert!(NntpResponse::parse(b"200 OK\r\n").is_success());
    assert!(NntpResponse::parse(b"281 Auth\r\n").is_success());
    assert!(NntpResponse::parse(b"381 Password required\r\n").is_success());
    assert!(!NntpResponse::parse(b"400 Error\r\n").is_success());
    assert!(!NntpResponse::parse(b"500 Error\r\n").is_success());
    assert!(!NntpResponse::Invalid.is_success());
}

#[test]
fn test_response_status_code_extraction() {
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

// Multiline status code detection on StatusCode itself

#[test]
fn test_status_code_multiline_detection() {
    // All 1xx are multiline
    assert!(StatusCode::new(100).is_multiline());
    assert!(StatusCode::new(111).is_multiline());
    assert!(StatusCode::new(199).is_multiline());

    // Specific 2xx multiline codes
    assert!(StatusCode::new(220).is_multiline());
    assert!(StatusCode::new(221).is_multiline());
    assert!(StatusCode::new(222).is_multiline());

    // Non-multiline
    assert!(!StatusCode::new(200).is_multiline());
    assert!(!StatusCode::new(211).is_multiline());
    assert!(!StatusCode::new(400).is_multiline());
    assert!(!StatusCode::new(500).is_multiline());
}

// Response with multiline content

#[test]
fn test_response_with_multiline_data_parses_code() {
    // Even with additional data, the first 3 bytes determine the code
    let data = b"220 12345 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";
    let response = NntpResponse::parse(data);
    assert!(matches!(response, NntpResponse::MultilineData(_)));
    assert_eq!(response.status_code(), Some(StatusCode::new(220)));
}
