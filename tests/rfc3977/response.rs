//! RFC 3977 Section 3.2 - Response Code Classification Tests
//!
//! These tests verify correct classification of NNTP response codes.

use nntp_proxy::protocol::{NntpResponse, StatusCode};

fn status(code: u16) -> StatusCode {
    StatusCode::new(code)
}

fn line(code: u16) -> String {
    format!("{code} Test\r\n")
}

fn parsed(code: u16) -> NntpResponse {
    NntpResponse::parse(line(code).as_bytes())
}

fn assert_status_flags(
    code: u16,
    success: bool,
    error: bool,
    informational: bool,
    continuation: bool,
) {
    let status = status(code);
    assert_eq!(status.is_success(), success, "success {code}");
    assert_eq!(status.is_error(), error, "error {code}");
    assert_eq!(
        status.is_informational(),
        informational,
        "informational {code}"
    );
    assert_eq!(
        status.is_continuation(),
        continuation,
        "continuation {code}"
    );
}

fn assert_multiline(code: u16, multiline: bool) {
    assert_eq!(
        parsed(code).status_implies_multiline(),
        multiline,
        "multiline {code}"
    );
}

// StatusCode classification

#[test]
fn test_status_code_categories_and_boundaries() {
    [
        (100, false, false, true, false),
        (199, false, false, true, false),
        (200, true, false, false, false),
        (299, true, false, false, false),
        (300, true, false, false, true),
        (335, true, false, false, true),
        (381, true, false, false, true),
        (399, true, false, false, true),
        (400, false, true, false, false),
        (430, false, true, false, false),
        (499, false, true, false, false),
        (500, false, true, false, false),
    ]
    .into_iter()
    .for_each(|(code, success, error, informational, continuation)| {
        assert_status_flags(code, success, error, informational, continuation);
    });
}

#[test]
fn test_known_success_and_continuation_codes() {
    [
        200, 201, 205, 211, 215, 220, 221, 222, 223, 224, 225, 230, 231, 281, 282,
    ]
    .into_iter()
    .for_each(|code| {
        let status = status(code);
        assert!(status.is_success(), "Code {code} should be success");
        assert!(!status.is_error(), "Code {code} should not be error");
    });

    [335, 340, 381, 383].into_iter().for_each(|code| {
        let status = status(code);
        assert!(
            status.is_continuation(),
            "Code {code} should be continuation"
        );
        assert!(status.is_success(), "Code {code} should also be success");
    });
}

// StatusCode::parse() tests

#[test]
fn test_status_code_parse_valid_inputs() {
    [
        (b"200 Service ready\r\n".as_slice(), 200),
        (b"200".as_slice(), 200),
        (b"200 ".as_slice(), 200),
        (b"000".as_slice(), 0),
        (b"999".as_slice(), 999),
        (b"211 42 1 100 alt.test".as_slice(), 211),
        (b"200 Welcome! <server@example>\r\n".as_slice(), 200),
        (b"200  multiple  spaces  \r\n".as_slice(), 200),
        ("200 Привет мир\r\n".as_bytes(), 200),
    ]
    .into_iter()
    .for_each(|(input, code)| assert_eq!(StatusCode::parse(input), Some(status(code))));

    let long_msg = format!("200 {}\r\n", "x".repeat(1000));
    assert_eq!(StatusCode::parse(long_msg.as_bytes()), Some(status(200)));
}

#[test]
fn test_status_code_parse_invalid_inputs() {
    [
        b"".as_slice(),
        b"20".as_slice(),
        b"2".as_slice(),
        b"2X0 Error\r\n".as_slice(),
        b"ABC Invalid\r\n".as_slice(),
        b" 200 Error\r\n".as_slice(),
    ]
    .into_iter()
    .for_each(|input| assert_eq!(StatusCode::parse(input), None));
}

// NntpResponse categorization

#[test]
fn test_nntp_response_special_categories() {
    assert!(matches!(parsed(200), NntpResponse::Greeting(_)));
    assert!(matches!(parsed(201), NntpResponse::Greeting(_)));
    assert_eq!(
        NntpResponse::parse(b"205 Bye\r\n"),
        NntpResponse::Disconnect
    );
    assert_eq!(
        NntpResponse::parse(b"281 OK\r\n"),
        NntpResponse::AuthSuccess
    );
    assert!(matches!(parsed(381), NntpResponse::AuthRequired(_)));
    assert!(matches!(parsed(480), NntpResponse::AuthRequired(_)));
    assert!(matches!(parsed(400), NntpResponse::SingleLine(_)));
    assert!(matches!(parsed(430), NntpResponse::SingleLine(_)));
    assert!(matches!(parsed(500), NntpResponse::SingleLine(_)));
    assert_eq!(NntpResponse::parse(b""), NntpResponse::Invalid);
    assert_eq!(NntpResponse::parse(b"XXX\r\n"), NntpResponse::Invalid);
}

#[test]
fn test_response_success_method() {
    [200, 281, 381]
        .into_iter()
        .for_each(|code| assert!(parsed(code).is_success(), "success {code}"));
    [400, 500]
        .into_iter()
        .for_each(|code| assert!(!parsed(code).is_success(), "not success {code}"));
    assert!(!NntpResponse::Invalid.is_success());
}

#[test]
fn test_response_status_code_extraction() {
    [
        (NntpResponse::parse(b"200 OK\r\n"), Some(status(200))),
        (NntpResponse::Disconnect, Some(status(205))),
        (NntpResponse::AuthSuccess, Some(status(281))),
        (NntpResponse::Invalid, None),
    ]
    .into_iter()
    .for_each(|(response, expected)| assert_eq!(response.status_code(), expected));
}

// Multiline classification remains response categorization, not a StatusCode property.

#[test]
fn test_response_multiline_detection() {
    [
        (100, true),
        (101, true),
        (111, false),
        (199, false),
        (200, false),
        (201, false),
        (205, false),
        (211, false),
        (215, true),
        (220, true),
        (221, true),
        (222, true),
        (224, true),
        (225, true),
        (230, true),
        (231, true),
        (281, false),
        (282, true),
        (400, false),
        (500, false),
    ]
    .into_iter()
    .for_each(|(code, multiline)| assert_multiline(code, multiline));
}

#[test]
fn test_response_with_multiline_data_parses_code() {
    let data = b"220 12345 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";
    let response = NntpResponse::parse(data);
    assert!(matches!(response, NntpResponse::MultilineData(_)));
    assert_eq!(response.status_code(), Some(status(220)));
}
