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
use nntp_proxy::types::MessageId;

fn msgid(value: &str) -> MessageId<'_> {
    MessageId::from_borrowed(value).unwrap()
}

fn wire(context: &protocol::RequestContext) -> Vec<u8> {
    let mut out = Vec::with_capacity(context.wire_len());
    out.extend_from_slice(context.verb());
    if !context.args().is_empty() {
        out.push(b' ');
        out.extend_from_slice(context.args());
    }
    out.extend_from_slice(b"\r\n");
    out
}

// === Command Termination (RFC 3977 §3.1) ===

#[test]
fn test_all_commands_end_with_crlf() {
    // RFC 3977 §3.1: "Commands in NNTP MUST use the canonical CRLF"
    let test_commands = vec![
        protocol::authinfo_user("user").into_bytes(),
        protocol::authinfo_pass("pass").into_bytes(),
        wire(&protocol::article_request(&msgid("<msgid@example>"))),
        wire(&protocol::body_request(&msgid("<msgid@example>"))),
        wire(&protocol::head_request(&msgid("<msgid@example>"))),
        wire(&protocol::stat_request(&msgid("<msgid@example>"))),
    ];

    for cmd in &test_commands {
        assert!(
            cmd.ends_with(b"\r\n"),
            "Command does not end with CRLF: {cmd:?}"
        );
    }

    // Constants too
    assert!(protocol::QUIT.ends_with(b"\r\n"));
    assert!(wire(&protocol::date_request()).ends_with(b"\r\n"));
}

#[test]
fn test_commands_only_one_crlf() {
    // Commands should have exactly one CRLF - prevents command injection
    let commands = vec![
        protocol::authinfo_user("testuser").into_bytes(),
        protocol::authinfo_pass("testpass").into_bytes(),
        wire(&protocol::article_request(&msgid("<test@example>"))),
        wire(&protocol::body_request(&msgid("<test@example>"))),
        wire(&protocol::head_request(&msgid("<test@example>"))),
        wire(&protocol::stat_request(&msgid("<test@example>"))),
    ];

    for cmd in &commands {
        assert_eq!(
            cmd.windows(2).filter(|window| *window == b"\r\n").count(),
            1,
            "Command has multiple CRLFs (injection risk): {cmd:?}"
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
    let cmd = protocol::article_request(&msgid("<unique-id@news.example.com>"));
    assert_eq!(wire(&cmd), b"ARTICLE <unique-id@news.example.com>\r\n");
}

#[test]
fn test_body_command_with_message_id() {
    let cmd = protocol::body_request(&msgid("<test@example.com>"));
    assert_eq!(wire(&cmd), b"BODY <test@example.com>\r\n");
}

#[test]
fn test_head_command_with_message_id() {
    let cmd = protocol::head_request(&msgid("<test@example.com>"));
    assert_eq!(wire(&cmd), b"HEAD <test@example.com>\r\n");
}

#[test]
fn test_stat_command_with_message_id() {
    let cmd = protocol::stat_request(&msgid("<test@example.com>"));
    assert_eq!(wire(&cmd), b"STAT <test@example.com>\r\n");
}

// === Command Constants ===

#[test]
fn test_quit_constant() {
    assert_eq!(protocol::QUIT, b"QUIT\r\n");
}

#[test]
fn test_date_constant() {
    assert_eq!(wire(&protocol::date_request()), b"DATE\r\n");
}

// === Keywords Are Uppercase ===

#[test]
fn test_keywords_are_uppercase() {
    // RFC 3977 §3.1: Keywords are case-insensitive, convention is UPPERCASE
    assert!(protocol::authinfo_user("u").starts_with("AUTHINFO USER "));
    assert!(protocol::authinfo_pass("p").starts_with("AUTHINFO PASS "));
    assert!(wire(&protocol::article_request(&msgid("<m>"))).starts_with(b"ARTICLE "));
    assert!(wire(&protocol::body_request(&msgid("<m>"))).starts_with(b"BODY "));
    assert!(wire(&protocol::head_request(&msgid("<m>"))).starts_with(b"HEAD "));
    assert!(wire(&protocol::stat_request(&msgid("<m>"))).starts_with(b"STAT "));
    assert!(protocol::QUIT.starts_with(b"QUIT"));
    assert!(wire(&protocol::date_request()).starts_with(b"DATE"));
}

// === Command Length (RFC 3977 §3.1) ===

#[test]
fn test_standard_commands_under_512_octets() {
    // RFC 3977 §3.1: "command line MUST NOT exceed 512 octets"
    let commands = vec![
        protocol::authinfo_user("typical_username"),
        protocol::authinfo_pass("typical_password"),
        String::from_utf8(wire(&protocol::article_request(&msgid(
            "<typical-msgid@example.com>",
        ))))
        .unwrap(),
        String::from_utf8(wire(&protocol::body_request(&msgid(
            "<typical-msgid@example.com>",
        ))))
        .unwrap(),
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
    let cmd = protocol::article_request(&msgid(&long_id));
    // We produce the command; server will reject if too long
    assert!(cmd.wire_len() > 0);
    assert!(wire(&cmd).ends_with(b"\r\n"));
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
        NntpCommand::Capabilities
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
    // POST — RFC 3977 §6.3.1: posting not permitted → dedicated Post variant
    assert!(matches!(NntpCommand::parse("POST"), NntpCommand::Post));
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
    // NEWGROUPS/NEWNEWS are read-only queries (RFC 3977 §7.3-7.4)
    assert!(matches!(
        NntpCommand::parse("newgroups 20240101 000000"),
        NntpCommand::Stateless
    ));
    assert!(matches!(
        NntpCommand::parse("newnews * 20240101 000000"),
        NntpCommand::Stateless
    ));
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
    // POST — RFC 3977 §6.3.1: dedicated Post variant, case-insensitive
    assert!(matches!(NntpCommand::parse("post"), NntpCommand::Post));
    assert!(matches!(
        NntpCommand::parse("ihave <msgid@example>"),
        NntpCommand::NonRoutable
    ));
}
