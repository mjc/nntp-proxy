//! RFC 3977 Section 3.2 - Response Code Classification Tests
//!
//! These tests verify correct classification of NNTP response codes.

use nntp_proxy::protocol::StatusCode;

fn status(code: u16) -> StatusCode {
    StatusCode::new(code)
}

#[derive(Clone, Copy)]
enum StatusCategory {
    Informational,
    Success,
    Continuation,
    Error,
}

const STATUS_CATEGORY_CASES: &[(u16, StatusCategory)] = &[
    (100, StatusCategory::Informational),
    (199, StatusCategory::Informational),
    (200, StatusCategory::Success),
    (299, StatusCategory::Success),
    (300, StatusCategory::Continuation),
    (335, StatusCategory::Continuation),
    (381, StatusCategory::Continuation),
    (399, StatusCategory::Continuation),
    (400, StatusCategory::Error),
    (430, StatusCategory::Error),
    (499, StatusCategory::Error),
    (500, StatusCategory::Error),
];

fn assert_status_flags(code: u16, category: StatusCategory) {
    let status = status(code);
    match category {
        StatusCategory::Informational => {
            assert!(!status.is_success(), "success {code}");
            assert!(!status.is_error(), "error {code}");
            assert!(status.is_informational(), "informational {code}");
            assert!(!status.is_continuation(), "continuation {code}");
        }
        StatusCategory::Success => {
            assert!(status.is_success(), "success {code}");
            assert!(!status.is_error(), "error {code}");
            assert!(!status.is_informational(), "informational {code}");
            assert!(!status.is_continuation(), "continuation {code}");
        }
        StatusCategory::Continuation => {
            assert!(status.is_success(), "success {code}");
            assert!(!status.is_error(), "error {code}");
            assert!(!status.is_informational(), "informational {code}");
            assert!(status.is_continuation(), "continuation {code}");
        }
        StatusCategory::Error => {
            assert!(!status.is_success(), "success {code}");
            assert!(status.is_error(), "error {code}");
            assert!(!status.is_informational(), "informational {code}");
            assert!(!status.is_continuation(), "continuation {code}");
        }
    }
}

// StatusCode classification

#[test]
fn test_status_code_categories_and_boundaries() {
    for (code, category) in STATUS_CATEGORY_CASES {
        assert_status_flags(*code, *category);
    }
}

#[test]
fn test_known_success_and_continuation_codes() {
    for code in [
        200, 201, 205, 211, 215, 220, 221, 222, 223, 224, 225, 230, 231, 281, 282,
    ] {
        let status = status(code);
        assert!(status.is_success(), "Code {code} should be success");
        assert!(!status.is_error(), "Code {code} should not be error");
    }

    for code in [335, 340, 381, 383] {
        let status = status(code);
        assert!(
            status.is_continuation(),
            "Code {code} should be continuation"
        );
        assert!(status.is_success(), "Code {code} should also be success");
    }
}

// StatusCode::parse() tests

#[test]
fn test_status_code_parse_valid_inputs() {
    for (input, code) in [
        (b"200 Service ready\r\n".as_slice(), 200),
        (b"200".as_slice(), 200),
        (b"200 ".as_slice(), 200),
        (b"000".as_slice(), 0),
        (b"999".as_slice(), 999),
        (b"211 42 1 100 alt.test".as_slice(), 211),
        (b"200 Welcome! <server@example>\r\n".as_slice(), 200),
        (b"200  multiple  spaces  \r\n".as_slice(), 200),
        ("200 Привет мир\r\n".as_bytes(), 200),
    ] {
        assert_eq!(StatusCode::parse(input), Some(status(code)));
    }

    let long_msg = format!("200 {}\r\n", "x".repeat(1000));
    assert_eq!(StatusCode::parse(long_msg.as_bytes()), Some(status(200)));
}

#[test]
fn test_status_code_parse_invalid_inputs() {
    for input in [
        b"".as_slice(),
        b"20".as_slice(),
        b"2".as_slice(),
        b"2X0 Error\r\n".as_slice(),
        b"ABC Invalid\r\n".as_slice(),
        b" 200 Error\r\n".as_slice(),
    ] {
        assert_eq!(StatusCode::parse(input), None);
    }
}

// Setup/auth status helpers

#[test]
fn test_status_code_special_categories() {
    assert!(status(200).is_greeting());
    assert!(status(201).is_greeting());
    assert!(!status(205).is_greeting());
    assert!(status(281).is_auth_accepted());
    assert!(status(381).requires_auth_credentials());
    assert!(status(480).requires_auth_credentials());
    assert!(!status(481).requires_auth_credentials());
    assert!(status(430).is_article_missing());
}

#[test]
fn test_status_success_method() {
    for code in [200, 281, 381] {
        assert!(status(code).is_success(), "success {code}");
    }
    for code in [400, 500] {
        assert!(!status(code).is_success(), "not success {code}");
    }
}

#[test]
fn test_status_code_extraction() {
    for (response, expected) in [
        (b"200 OK\r\n".as_slice(), Some(status(200))),
        (b"205 Bye\r\n".as_slice(), Some(status(205))),
        (b"281 OK\r\n".as_slice(), Some(status(281))),
        (b"".as_slice(), None),
    ] {
        assert_eq!(StatusCode::parse(response), expected);
    }
}

#[test]
fn test_response_with_payload_parses_code_only() {
    let data = b"220 12345 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";
    assert_eq!(StatusCode::parse(data), Some(status(220)));
}
