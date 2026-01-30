//! RFC 3977 Section 3.1 - Command Format Tests
//!
//! These tests verify compliance with NNTP command format requirements:
//! - Commands terminate with CRLF
//! - Commands must not exceed 512 octets
//! - Keywords are case-insensitive (but we send uppercase)
//! - Command classification is correct
//!
//! Adapted from nntp-rs RFC 3977 commands.rs tests.

use nntp_proxy::command::NntpCommand;
use nntp_proxy::protocol;

// === Command Termination (RFC 3977 §3.1) ===

#[test]
fn test_all_commands_end_with_crlf() {
    // RFC 3977 §3.1: "Commands in NNTP MUST use the canonical CRLF"
    let test_commands: Vec<String> = vec![
        protocol::authinfo_user("user"),
        protocol::authinfo_pass("pass"),
        protocol::article_by_msgid("<msgid@example>"),
        protocol::body_by_msgid("<msgid@example>"),
        protocol::head_by_msgid("<msgid@example>"),
        protocol::stat_by_msgid("<msgid@example>"),
    ];

    for cmd in &test_commands {
        assert!(
            cmd.ends_with("\r\n"),
            "Command does not end with CRLF: {:?}",
            cmd
        );
    }

    // Constants too
    assert!(protocol::QUIT.ends_with(b"\r\n"));
    assert!(protocol::DATE.ends_with(b"\r\n"));
}

#[test]
fn test_commands_only_one_crlf() {
    // Commands should have exactly one CRLF - prevents command injection
    let commands = vec![
        protocol::authinfo_user("testuser"),
        protocol::authinfo_pass("testpass"),
        protocol::article_by_msgid("<test@example>"),
        protocol::body_by_msgid("<test@example>"),
        protocol::head_by_msgid("<test@example>"),
        protocol::stat_by_msgid("<test@example>"),
    ];

    for cmd in &commands {
        assert_eq!(
            cmd.matches("\r\n").count(),
            1,
            "Command has multiple CRLFs (injection risk): {:?}",
            cmd
        );
    }
}

// === AUTHINFO Commands (RFC 4643) ===

#[test]
fn test_authinfo_user_format() {
    let cmd = protocol::authinfo_user("testuser");
    assert_eq!(cmd, "AUTHINFO USER testuser\r\n");
}

#[test]
fn test_authinfo_user_with_spaces() {
    let cmd = protocol::authinfo_user("user name");
    assert_eq!(cmd, "AUTHINFO USER user name\r\n");
}

#[test]
fn test_authinfo_pass_with_special_chars() {
    let cmd = protocol::authinfo_pass("p@ss!w0rd#$%");
    assert_eq!(cmd, "AUTHINFO PASS p@ss!w0rd#$%\r\n");
}

// === ARTICLE/HEAD/BODY/STAT Commands (RFC 3977 §6.2) ===

#[test]
fn test_article_command_with_message_id() {
    let cmd = protocol::article_by_msgid("<unique-id@news.example.com>");
    assert_eq!(cmd, "ARTICLE <unique-id@news.example.com>\r\n");
}

#[test]
fn test_body_command_with_message_id() {
    let cmd = protocol::body_by_msgid("<test@example.com>");
    assert_eq!(cmd, "BODY <test@example.com>\r\n");
}

#[test]
fn test_head_command_with_message_id() {
    let cmd = protocol::head_by_msgid("<test@example.com>");
    assert_eq!(cmd, "HEAD <test@example.com>\r\n");
}

#[test]
fn test_stat_command_with_message_id() {
    let cmd = protocol::stat_by_msgid("<test@example.com>");
    assert_eq!(cmd, "STAT <test@example.com>\r\n");
}

// === Command Constants ===

#[test]
fn test_quit_constant() {
    assert_eq!(protocol::QUIT, b"QUIT\r\n");
}

#[test]
fn test_date_constant() {
    assert_eq!(protocol::DATE, b"DATE\r\n");
}

// === Keywords Are Uppercase ===

#[test]
fn test_keywords_are_uppercase() {
    // RFC 3977 §3.1: Keywords are case-insensitive, convention is UPPERCASE
    assert!(protocol::authinfo_user("u").starts_with("AUTHINFO USER "));
    assert!(protocol::authinfo_pass("p").starts_with("AUTHINFO PASS "));
    assert!(protocol::article_by_msgid("<m>").starts_with("ARTICLE "));
    assert!(protocol::body_by_msgid("<m>").starts_with("BODY "));
    assert!(protocol::head_by_msgid("<m>").starts_with("HEAD "));
    assert!(protocol::stat_by_msgid("<m>").starts_with("STAT "));
    assert!(protocol::QUIT.starts_with(b"QUIT"));
    assert!(protocol::DATE.starts_with(b"DATE"));
}

// === Command Length (RFC 3977 §3.1) ===

#[test]
fn test_standard_commands_under_512_octets() {
    // RFC 3977 §3.1: "command line MUST NOT exceed 512 octets"
    let commands = vec![
        protocol::authinfo_user("typical_username"),
        protocol::authinfo_pass("typical_password"),
        protocol::article_by_msgid("<typical-msgid@example.com>"),
        protocol::body_by_msgid("<typical-msgid@example.com>"),
    ];

    for cmd in &commands {
        assert!(
            cmd.len() <= 512,
            "Command exceeds 512 octets ({} bytes): {:?}",
            cmd.len(),
            cmd
        );
    }
}

#[test]
fn test_long_message_id_command_length() {
    // Very long message-ID may exceed 512 but the library doesn't enforce
    let long_id = format!("<{}@example.com>", "x".repeat(450));
    let cmd = protocol::article_by_msgid(&long_id);
    // We produce the command; server will reject if too long
    assert!(!cmd.is_empty());
    assert!(cmd.ends_with("\r\n"));
}

// === Command Classification (NntpCommand::parse) ===

#[test]
fn test_classify_article_by_message_id() {
    assert!(matches!(
        NntpCommand::parse("ARTICLE <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
    assert!(matches!(
        NntpCommand::parse("BODY <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
    assert!(matches!(
        NntpCommand::parse("HEAD <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
    assert!(matches!(
        NntpCommand::parse("STAT <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
}

#[test]
fn test_classify_article_by_number_is_stateful() {
    assert!(matches!(
        NntpCommand::parse("ARTICLE 12345"),
        NntpCommand::Stateful
    ));
    assert!(matches!(
        NntpCommand::parse("BODY 12345"),
        NntpCommand::Stateful
    ));
}

#[test]
fn test_classify_case_insensitive() {
    // RFC 3977 §3.1: Keywords are case-insensitive
    assert!(matches!(
        NntpCommand::parse("article <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
    assert!(matches!(
        NntpCommand::parse("Article <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
    assert!(matches!(
        NntpCommand::parse("ARTICLE <test@example.com>"),
        NntpCommand::ArticleByMessageId
    ));
}

#[test]
fn test_classify_auth_commands() {
    assert!(matches!(
        NntpCommand::parse("AUTHINFO USER testuser"),
        NntpCommand::AuthUser
    ));
    assert!(matches!(
        NntpCommand::parse("AUTHINFO PASS testpass"),
        NntpCommand::AuthPass
    ));
}

#[test]
fn test_classify_stateless_commands() {
    assert!(matches!(NntpCommand::parse("QUIT"), NntpCommand::Stateless));
    assert!(matches!(NntpCommand::parse("DATE"), NntpCommand::Stateless));
    assert!(matches!(NntpCommand::parse("LIST"), NntpCommand::Stateless));
    assert!(matches!(NntpCommand::parse("HELP"), NntpCommand::Stateless));
    assert!(matches!(
        NntpCommand::parse("CAPABILITIES"),
        NntpCommand::Stateless
    ));
}

#[test]
fn test_classify_stateful_commands() {
    assert!(matches!(
        NntpCommand::parse("GROUP alt.test"),
        NntpCommand::Stateful
    ));
    assert!(matches!(NntpCommand::parse("NEXT"), NntpCommand::Stateful));
    assert!(matches!(NntpCommand::parse("LAST"), NntpCommand::Stateful));
    assert!(matches!(
        NntpCommand::parse("XOVER 1-100"),
        NntpCommand::Stateful
    ));
}

#[test]
fn test_classify_non_routable_commands() {
    assert!(matches!(
        NntpCommand::parse("POST"),
        NntpCommand::NonRoutable
    ));
    assert!(matches!(
        NntpCommand::parse("IHAVE <msgid@example>"),
        NntpCommand::NonRoutable
    ));
}

#[test]
fn test_classify_empty_command() {
    // Empty input shouldn't panic
    let _ = NntpCommand::parse("");
}

#[test]
fn test_classify_whitespace_only() {
    let _ = NntpCommand::parse("   ");
}

#[test]
fn test_classify_unknown_command() {
    // Unknown commands fall through to a default classification
    let cmd = NntpCommand::parse("XYZZY");
    // Should not panic; exact classification depends on implementation
    let _ = cmd;
}

#[test]
fn test_classify_case_insensitive_auth() {
    assert!(matches!(
        NntpCommand::parse("authinfo user test"),
        NntpCommand::AuthUser
    ));
    assert!(matches!(
        NntpCommand::parse("Authinfo Pass test"),
        NntpCommand::AuthPass
    ));
}

#[test]
fn test_classify_case_insensitive_stateless() {
    assert!(matches!(NntpCommand::parse("quit"), NntpCommand::Stateless));
    assert!(matches!(NntpCommand::parse("list"), NntpCommand::Stateless));
    assert!(matches!(NntpCommand::parse("help"), NntpCommand::Stateless));
    assert!(matches!(NntpCommand::parse("date"), NntpCommand::Stateless));
}

#[test]
fn test_classify_case_insensitive_stateful() {
    assert!(matches!(
        NntpCommand::parse("group alt.test"),
        NntpCommand::Stateful
    ));
    assert!(matches!(NntpCommand::parse("next"), NntpCommand::Stateful));
    assert!(matches!(NntpCommand::parse("last"), NntpCommand::Stateful));
}

#[test]
fn test_classify_case_insensitive_non_routable() {
    assert!(matches!(
        NntpCommand::parse("post"),
        NntpCommand::NonRoutable
    ));
    assert!(matches!(
        NntpCommand::parse("ihave <msgid@example>"),
        NntpCommand::NonRoutable
    ));
}
