//! RFC 3977 Section 3.2.1 - NNTP Error Response Tests
//!
//! These tests verify correct handling of NNTP 4xx and 5xx response codes.

use nntp_proxy::protocol::{StatusCode, codes};

const TEMPORARY_ERRORS: &[(u16, u16)] = &[
    (codes::SERVICE_UNAVAILABLE, 400),
    (codes::INTERNAL_FAULT, 403),
    (codes::NO_SUCH_GROUP, 411),
    (codes::NO_GROUP_SELECTED, 412),
    (codes::NO_CURRENT_ARTICLE, 420),
    (codes::NO_NEXT_ARTICLE, 421),
    (codes::NO_PREV_ARTICLE, 422),
    (codes::NO_SUCH_ARTICLE_NUMBER, 423),
    (codes::NO_SUCH_ARTICLE_ID, 430),
    (codes::AUTH_REQUIRED, 480),
    (codes::AUTH_REJECTED, 481),
    (codes::AUTH_OUT_OF_SEQUENCE, 482),
    (codes::ENCRYPTION_REQUIRED, 483),
];

const PERMANENT_ERRORS: &[(u16, u16)] = &[
    (codes::COMMAND_NOT_RECOGNIZED, 500),
    (codes::COMMAND_SYNTAX_ERROR, 501),
    (codes::ACCESS_DENIED, 502),
    (codes::FEATURE_NOT_SUPPORTED, 503),
];

fn error_codes() -> impl Iterator<Item = u16> {
    TEMPORARY_ERRORS
        .iter()
        .chain(PERMANENT_ERRORS.iter())
        .map(|(constant, _)| *constant)
}

#[test]
fn test_error_code_constants_match_expected_values() {
    TEMPORARY_ERRORS
        .iter()
        .chain(PERMANENT_ERRORS.iter())
        .for_each(|(constant, expected)| assert_eq!(constant, expected));
}

#[test]
fn test_known_error_codes_are_errors() {
    error_codes().for_each(|code| {
        let status = StatusCode::new(code);
        assert!(status.is_error(), "Code {code} should be an error");
        assert!(!status.is_success(), "Code {code} should not be success");
        assert!(
            !status.is_continuation(),
            "Code {code} should not be continuation"
        );
        assert!(
            !status.is_informational(),
            "Code {code} should not be informational"
        );
    });
}

#[test]
fn test_error_responses_are_errors() {
    error_codes().for_each(|code| {
        let status = StatusCode::new(code);
        assert!(status.is_error(), "Error code {code} should be error");
        assert!(
            !status.is_success(),
            "Error code {code} should not be success"
        );
    });
}

#[test]
fn test_error_boundaries() {
    [
        (399, true, false, true),
        (400, false, true, false),
        (499, false, true, false),
        (500, false, true, false),
        (599, false, true, false),
    ]
    .into_iter()
    .for_each(|(code, continuation, error, success)| {
        let status = StatusCode::new(code);
        assert_eq!(
            status.is_continuation(),
            continuation,
            "continuation {code}"
        );
        assert_eq!(status.is_error(), error, "error {code}");
        assert_eq!(status.is_success(), success, "success {code}");
    });
}

#[test]
fn test_auth_error_special_cases() {
    assert!(StatusCode::new(480).requires_auth_credentials());
    assert!(!StatusCode::new(481).requires_auth_credentials());
}
