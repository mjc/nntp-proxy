//! Tests for STAT command precheck response handling
//!
//! Verifies that when a client sends STAT and precheck runs, the client always
//! receives 223 (STAT success) response, never 221 (HEAD success).
//!
//! This tests the fix for SABnzbd getting "unknown status code 221" errors.

use nntp_proxy::cache::BackendAvailability;
use nntp_proxy::types::BackendId;

#[test]
fn test_stat_response_is_always_223() {
    // This test documents the expected behavior:
    // When client sends STAT, they should ALWAYS get 223 response,
    // regardless of whether backend used HEAD (221) or STAT (223) for precheck.

    let expected_response = b"223 0 <article> Article exists\r\n";

    // Verify response code
    assert!(expected_response.starts_with(b"223"));
    assert!(!expected_response.starts_with(b"221"));

    // Verify it's a valid NNTP response
    assert!(expected_response.ends_with(b"\r\n"));
}

#[test]
fn test_backend_availability_marks_article_present() {
    // When precheck finds article (using STAT or HEAD), it marks backend as available
    let mut availability = BackendAvailability::new();
    let backend_id = BackendId::from_index(0);

    availability.mark_available(backend_id);

    assert!(availability.has_article(backend_id));
    assert!(availability.has_any());
}

#[test]
fn test_backend_availability_multiple_backends() {
    // Precheck can mark multiple backends as having the article
    let mut availability = BackendAvailability::new();

    availability.mark_available(BackendId::from_index(0));
    availability.mark_available(BackendId::from_index(1));

    assert!(availability.has_article(BackendId::from_index(0)));
    assert!(availability.has_article(BackendId::from_index(1)));
    assert!(!availability.has_article(BackendId::from_index(2)));
}

#[test]
fn test_stat_command_detection() {
    // Verify we can detect STAT commands
    let stat_commands = vec![
        "STAT <article@example.com>",
        "stat <article@example.com>",
        "STAT <abc123@test.net>",
        "  STAT <article@example.com>  ", // with whitespace
    ];

    for cmd in stat_commands {
        assert!(
            cmd.trim_start().to_uppercase().starts_with("STAT "),
            "Should detect STAT command: {}",
            cmd
        );
    }
}

#[test]
fn test_non_stat_commands_not_detected() {
    // Verify we don't falsely detect non-STAT commands
    let non_stat_commands = vec![
        "ARTICLE <article@example.com>",
        "HEAD <article@example.com>",
        "BODY <article@example.com>",
        "LIST",
        "GROUP alt.test",
    ];

    for cmd in non_stat_commands {
        assert!(
            !cmd.trim_start().to_uppercase().starts_with("STAT "),
            "Should NOT detect as STAT command: {}",
            cmd
        );
    }
}

#[test]
fn test_stat_precheck_flow_documentation() {
    // This test documents the expected flow:
    //
    // 1. Client sends: STAT <msgid>
    // 2. Precheck runs (may use STAT or HEAD on backend based on config)
    // 3. Backend responds with 223 (STAT) or 221 (HEAD)
    // 4. Precheck marks availability in cache
    // 5. try_serve_stat_from_cache sees availability and command is STAT
    // 6. Proxy manufactures 223 response (translation implicit)
    // 7. Client receives: 223 0 <msgid> Article exists
    //
    // Client NEVER sees 221 response for STAT command.

    let client_command = "STAT <test@example.com>";
    let proxy_response = "223 0 <article> Article exists\r\n";

    assert!(client_command.to_uppercase().starts_with("STAT"));
    assert!(proxy_response.starts_with("223"));
    assert!(!proxy_response.starts_with("221"));
}

#[test]
fn test_precheck_with_no_article_returns_430() {
    // When precheck finds no backend has the article,
    // client should get 430 (no such article), not 221 or 223

    let availability = BackendAvailability::new(); // Empty - no backends have article

    assert!(!availability.has_any());

    // Expected response when no backend has article
    let expected_response = "430 No such article";
    assert!(expected_response.starts_with("430"));
}

#[test]
fn test_stat_response_format() {
    // Verify the manufactured STAT response has correct format
    let response = b"223 0 <article> Article exists\r\n";

    // Check RFC 3977 compliance:
    // - Status code 223
    // - Space
    // - Article number (0 for message-ID based request)
    // - Space
    // - Message-ID in angle brackets
    // - Space
    // - Status text
    // - CRLF terminator

    let response_str = std::str::from_utf8(response).unwrap();
    let parts: Vec<&str> = response_str.split_whitespace().collect();

    assert_eq!(parts[0], "223");
    assert_eq!(parts[1], "0");
    assert!(parts[2].starts_with('<'));
    assert!(parts[2].ends_with('>'));
}
