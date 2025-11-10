//! Tests for message-ID extraction and validation

use crate::protocol::response::*;

#[test]
fn test_extract_message_id() {
    // Standard message-ID
    assert_eq!(
        NntpResponse::extract_message_id("ARTICLE <test@example.com>").map(|id| id.to_string()),
        Some("<test@example.com>".to_string())
    );

    // With extra whitespace
    assert_eq!(
        NntpResponse::extract_message_id("  BODY  <msg123@news.com>  ").map(|id| id.to_string()),
        Some("<msg123@news.com>".to_string())
    );

    // No message-ID
    assert_eq!(NntpResponse::extract_message_id("ARTICLE 123"), None);

    // Malformed (no closing >)
    assert_eq!(
        NntpResponse::extract_message_id("ARTICLE <test@example.com"),
        None
    );

    // Multiple message-IDs (returns first)
    assert_eq!(
        NntpResponse::extract_message_id("TEST <first@example.com> <second@example.com>")
            .map(|id| id.to_string()),
        Some("<first@example.com>".to_string())
    );
}

#[test]
fn test_validate_message_id() {
    // Valid message-IDs
    assert!(NntpResponse::validate_message_id("<test@example.com>"));
    assert!(NntpResponse::validate_message_id(
        "<msg123@news.server.com>"
    ));
    assert!(NntpResponse::validate_message_id("<a@b>"));
    assert!(NntpResponse::validate_message_id("  <valid@test.com>  "));

    // Invalid: missing brackets
    assert!(!NntpResponse::validate_message_id("test@example.com"));
    assert!(!NntpResponse::validate_message_id("<test@example.com"));
    assert!(!NntpResponse::validate_message_id("test@example.com>"));

    // Invalid: missing @
    assert!(!NntpResponse::validate_message_id("<testexample.com>"));

    // Invalid: multiple @
    assert!(!NntpResponse::validate_message_id("<test@example@com>"));

    // Invalid: @ at start or end
    assert!(!NntpResponse::validate_message_id("<@example.com>"));
    assert!(!NntpResponse::validate_message_id("<test@>"));

    // Invalid: empty
    assert!(!NntpResponse::validate_message_id(""));
    assert!(!NntpResponse::validate_message_id("<>"));

    // Invalid: only brackets
    assert!(!NntpResponse::validate_message_id("<@>"));
}
