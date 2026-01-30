//! RFC 3977 Section 3.2.1 - NNTP Error Response Tests
//!
//! These tests verify correct handling of all NNTP error codes:
//! - 4xx: Temporary errors
//! - 5xx: Permanent errors
//!
//! Adapted from nntp-rs RFC 3977 errors.rs tests.

use nntp_proxy::protocol::{NntpResponse, StatusCode, codes};

// === 4xx Temporary Error Codes ===

#[test]
fn test_error_400_service_unavailable() {
    let code = StatusCode::new(codes::SERVICE_UNAVAILABLE);
    assert_eq!(code.as_u16(), 400);
    assert!(code.is_error());
    assert!(!code.is_success());
}

#[test]
fn test_error_403_internal_fault() {
    let code = StatusCode::new(codes::INTERNAL_FAULT);
    assert_eq!(code.as_u16(), 403);
    assert!(code.is_error());
}

#[test]
fn test_error_411_no_such_group() {
    let code = StatusCode::new(codes::NO_SUCH_GROUP);
    assert_eq!(code.as_u16(), 411);
    assert!(code.is_error());
}

#[test]
fn test_error_412_no_group_selected() {
    let code = StatusCode::new(codes::NO_GROUP_SELECTED);
    assert_eq!(code.as_u16(), 412);
    assert!(code.is_error());
}

#[test]
fn test_error_420_no_current_article() {
    let code = StatusCode::new(codes::NO_CURRENT_ARTICLE);
    assert_eq!(code.as_u16(), 420);
    assert!(code.is_error());
}

#[test]
fn test_error_421_no_next_article() {
    let code = StatusCode::new(codes::NO_NEXT_ARTICLE);
    assert_eq!(code.as_u16(), 421);
    assert!(code.is_error());
}

#[test]
fn test_error_422_no_previous_article() {
    let code = StatusCode::new(codes::NO_PREV_ARTICLE);
    assert_eq!(code.as_u16(), 422);
    assert!(code.is_error());
}

#[test]
fn test_error_423_no_such_article_number() {
    let code = StatusCode::new(codes::NO_SUCH_ARTICLE_NUMBER);
    assert_eq!(code.as_u16(), 423);
    assert!(code.is_error());
}

#[test]
fn test_error_430_no_such_article_id() {
    let code = StatusCode::new(codes::NO_SUCH_ARTICLE_ID);
    assert_eq!(code.as_u16(), 430);
    assert!(code.is_error());
}

#[test]
fn test_error_480_auth_required() {
    let code = StatusCode::new(codes::AUTH_REQUIRED);
    assert_eq!(code.as_u16(), 480);
    assert!(code.is_error());
}

#[test]
fn test_error_481_auth_rejected() {
    let code = StatusCode::new(codes::AUTH_REJECTED);
    assert_eq!(code.as_u16(), 481);
    assert!(code.is_error());
}

#[test]
fn test_error_482_auth_out_of_sequence() {
    let code = StatusCode::new(codes::AUTH_OUT_OF_SEQUENCE);
    assert_eq!(code.as_u16(), 482);
    assert!(code.is_error());
}

#[test]
fn test_error_483_encryption_required() {
    let code = StatusCode::new(codes::ENCRYPTION_REQUIRED);
    assert_eq!(code.as_u16(), 483);
    assert!(code.is_error());
}

// === 5xx Permanent Error Codes ===

#[test]
fn test_error_500_command_not_recognized() {
    let code = StatusCode::new(codes::COMMAND_NOT_RECOGNIZED);
    assert_eq!(code.as_u16(), 500);
    assert!(code.is_error());
}

#[test]
fn test_error_501_syntax_error() {
    let code = StatusCode::new(codes::COMMAND_SYNTAX_ERROR);
    assert_eq!(code.as_u16(), 501);
    assert!(code.is_error());
}

#[test]
fn test_error_502_access_denied() {
    let code = StatusCode::new(codes::ACCESS_DENIED);
    assert_eq!(code.as_u16(), 502);
    assert!(code.is_error());
}

#[test]
fn test_error_503_feature_not_supported() {
    let code = StatusCode::new(codes::FEATURE_NOT_SUPPORTED);
    assert_eq!(code.as_u16(), 503);
    assert!(code.is_error());
}

// === Batch Error Classification ===

#[test]
fn test_all_4xx_codes_are_errors() {
    let codes_4xx = [
        400, 403, 411, 412, 420, 421, 422, 423, 430, 480, 481, 482, 483,
    ];

    for code in codes_4xx {
        let sc = StatusCode::new(code);
        assert!(sc.is_error(), "Code {} should be an error", code);
        assert!(!sc.is_success(), "Code {} should not be success", code);
        assert!(
            !sc.is_continuation(),
            "Code {} should not be continuation",
            code
        );
        assert!(
            !sc.is_informational(),
            "Code {} should not be informational",
            code
        );
    }
}

#[test]
fn test_all_5xx_codes_are_errors() {
    let codes_5xx = [500, 501, 502, 503];

    for code in codes_5xx {
        let sc = StatusCode::new(code);
        assert!(sc.is_error(), "Code {} should be an error", code);
        assert!(!sc.is_success(), "Code {} should not be success", code);
    }
}

// === Error Code Response Parsing ===

#[test]
fn test_error_responses_parse_as_single_line() {
    let error_codes = [
        400, 403, 411, 412, 420, 421, 422, 423, 430, 480, 481, 482, 483, 500, 501, 502, 503,
    ];

    for code in error_codes {
        let data = format!("{} Error message\r\n", code);
        let response = NntpResponse::parse(data.as_bytes());
        assert!(
            matches!(
                response,
                NntpResponse::SingleLine(_) | NntpResponse::AuthRequired(_)
            ),
            "Code {} should parse as SingleLine or AuthRequired, got {:?}",
            code,
            response
        );
    }
}

// === Boundary Tests ===

#[test]
fn test_boundary_between_continuation_and_error() {
    // 399 is continuation (3xx)
    let code_399 = StatusCode::new(399);
    assert!(code_399.is_continuation());
    assert!(!code_399.is_error());

    // 400 is error (4xx)
    let code_400 = StatusCode::new(400);
    assert!(!code_400.is_continuation());
    assert!(code_400.is_error());
}

#[test]
fn test_boundary_between_4xx_and_5xx() {
    // Both are errors
    let code_499 = StatusCode::new(499);
    assert!(code_499.is_error());

    let code_500 = StatusCode::new(500);
    assert!(code_500.is_error());
}

#[test]
fn test_boundary_at_599() {
    let code_599 = StatusCode::new(599);
    assert!(code_599.is_error());
    assert!(!code_599.is_success());
}

// === Error Constants Match Expected Values ===

#[test]
fn test_all_error_code_constants() {
    // 4xx codes
    assert_eq!(codes::SERVICE_UNAVAILABLE, 400);
    assert_eq!(codes::INTERNAL_FAULT, 403);
    assert_eq!(codes::NO_SUCH_GROUP, 411);
    assert_eq!(codes::NO_GROUP_SELECTED, 412);
    assert_eq!(codes::NO_CURRENT_ARTICLE, 420);
    assert_eq!(codes::NO_NEXT_ARTICLE, 421);
    assert_eq!(codes::NO_PREV_ARTICLE, 422);
    assert_eq!(codes::NO_SUCH_ARTICLE_NUMBER, 423);
    assert_eq!(codes::NO_SUCH_ARTICLE_ID, 430);
    assert_eq!(codes::AUTH_REQUIRED, 480);
    assert_eq!(codes::AUTH_REJECTED, 481);
    assert_eq!(codes::AUTH_OUT_OF_SEQUENCE, 482);
    assert_eq!(codes::ENCRYPTION_REQUIRED, 483);

    // 5xx codes
    assert_eq!(codes::COMMAND_NOT_RECOGNIZED, 500);
    assert_eq!(codes::COMMAND_SYNTAX_ERROR, 501);
    assert_eq!(codes::ACCESS_DENIED, 502);
    assert_eq!(codes::FEATURE_NOT_SUPPORTED, 503);
}

// === NntpResponse Error Handling ===

#[test]
fn test_error_response_not_multiline() {
    for code in [400, 430, 500, 502] {
        let data = format!("{} Error\r\n", code);
        let response = NntpResponse::parse(data.as_bytes());
        assert!(
            !response.is_multiline(),
            "Error code {} should not be multiline",
            code
        );
    }
}

#[test]
fn test_error_response_not_success() {
    for code in [400, 430, 500, 502] {
        let data = format!("{} Error\r\n", code);
        let response = NntpResponse::parse(data.as_bytes());
        assert!(
            !response.is_success(),
            "Error code {} should not be success",
            code
        );
    }
}

// === 480 special case: AuthRequired ===

#[test]
fn test_480_parsed_as_auth_required() {
    let response = NntpResponse::parse(b"480 Authentication required\r\n");
    assert!(matches!(response, NntpResponse::AuthRequired(_)));
}

#[test]
fn test_481_parsed_as_single_line() {
    // 481 is not AuthRequired - it's a rejection (error)
    let response = NntpResponse::parse(b"481 Authentication rejected\r\n");
    assert!(matches!(response, NntpResponse::SingleLine(_)));
}
