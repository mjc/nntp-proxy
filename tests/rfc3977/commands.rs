//! RFC 3977 Section 3.1 - Command Format Tests
//!
//! These tests verify compliance with NNTP command format requirements:
//! - Commands terminate with CRLF
//! - Commands must not exceed 512 octets
//! - Keywords are case-insensitive (but we send uppercase)
//! - Command classification is correct
//!
//! Adapted from nntp-rs RFC 3977 commands.rs tests.

use nntp_proxy::protocol;
use nntp_proxy::protocol::{RequestContext, RequestKind, RequestRouteClass};
use nntp_proxy::types::MessageId;

fn msgid(value: &str) -> MessageId<'_> {
    MessageId::from_borrowed(value).unwrap()
}

fn wire(context: &protocol::RequestContext) -> Vec<u8> {
    let mut out = Vec::with_capacity(context.request_wire_len().get());
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
        wire(&protocol::authinfo_user("user")),
        wire(&protocol::authinfo_pass("pass")),
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
        wire(&protocol::authinfo_user("testuser")),
        wire(&protocol::authinfo_pass("testpass")),
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
    assert_eq!(wire(&cmd), b"AUTHINFO USER testuser\r\n");
}

#[test]
fn test_authinfo_user_with_spaces() {
    let cmd = protocol::authinfo_user("user name");
    assert_eq!(wire(&cmd), b"AUTHINFO USER user name\r\n");
}

#[test]
fn test_authinfo_pass_with_special_chars() {
    let cmd = protocol::authinfo_pass("p@ss!w0rd#$%");
    assert_eq!(wire(&cmd), b"AUTHINFO PASS p@ss!w0rd#$%\r\n");
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
    assert!(wire(&protocol::authinfo_user("u")).starts_with(b"AUTHINFO USER "));
    assert!(wire(&protocol::authinfo_pass("p")).starts_with(b"AUTHINFO PASS "));
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
        String::from_utf8(wire(&protocol::authinfo_user("typical_username"))).unwrap(),
        String::from_utf8(wire(&protocol::authinfo_pass("typical_password"))).unwrap(),
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
    assert!(cmd.request_wire_len().get() > 0);
    assert!(wire(&cmd).ends_with(b"\r\n"));
}

// === Request Classification ===

#[test]
fn test_classify_article_by_message_id() {
    for line in [
        "ARTICLE <test@example.com>",
        "BODY <test@example.com>",
        "HEAD <test@example.com>",
        "STAT <test@example.com>",
    ] {
        assert_eq!(
            RequestContext::from_request_bytes(line.as_bytes()).route_class(),
            RequestRouteClass::ArticleByMessageId
        );
    }
}

#[test]
fn test_classify_article_by_number_is_stateful() {
    for line in ["ARTICLE 12345", "BODY 12345"] {
        assert_eq!(
            RequestContext::from_request_bytes(line.as_bytes()).route_class(),
            RequestRouteClass::Stateful
        );
    }
}

#[test]
fn test_classify_case_insensitive() {
    // RFC 3977 §3.1: Keywords are case-insensitive
    for line in [
        "article <test@example.com>",
        "Article <test@example.com>",
        "ARTICLE <test@example.com>",
    ] {
        let request = RequestContext::from_request_bytes(line.as_bytes());
        assert_eq!(request.kind(), RequestKind::Article);
        assert_eq!(request.route_class(), RequestRouteClass::ArticleByMessageId);
    }
}

#[test]
fn test_classify_auth_commands() {
    for line in ["AUTHINFO USER testuser", "AUTHINFO PASS testpass"] {
        let request = RequestContext::from_request_bytes(line.as_bytes());
        assert_eq!(request.kind(), RequestKind::AuthInfo);
        assert_eq!(request.route_class(), RequestRouteClass::Local);
    }
}

#[test]
fn test_classify_stateless_commands() {
    let cases = [
        ("QUIT", RequestKind::Quit, RequestRouteClass::Local),
        ("DATE", RequestKind::Date, RequestRouteClass::Stateless),
        ("LIST", RequestKind::List, RequestRouteClass::Stateless),
        ("HELP", RequestKind::Help, RequestRouteClass::Stateless),
        (
            "CAPABILITIES",
            RequestKind::Capabilities,
            RequestRouteClass::Local,
        ),
    ];
    for (line, kind, route_class) in cases {
        let request = RequestContext::from_request_bytes(line.as_bytes());
        assert_eq!(request.kind(), kind);
        assert_eq!(request.route_class(), route_class);
    }
}

#[test]
fn test_classify_stateful_commands() {
    for line in ["GROUP alt.test", "NEXT", "LAST", "XOVER 1-100"] {
        assert_eq!(
            RequestContext::from_request_bytes(line.as_bytes()).route_class(),
            RequestRouteClass::Stateful
        );
    }
}

#[test]
fn test_classify_non_routable_commands() {
    // POST — RFC 3977 §6.3.1: posting not permitted → dedicated Post variant
    for line in ["POST", "IHAVE <msgid@example>"] {
        assert_eq!(
            RequestContext::from_request_bytes(line.as_bytes()).route_class(),
            RequestRouteClass::Reject
        );
    }
}

#[test]
fn test_classify_empty_command() {
    // Empty input shouldn't panic
    let _ = RequestContext::from_request_bytes(b"");
}

#[test]
fn test_classify_whitespace_only() {
    let _ = RequestContext::from_request_bytes(b"   ");
}

#[test]
fn test_classify_unknown_command() {
    // Unknown commands fall through to a default classification
    let request = RequestContext::from_request_bytes(b"XYZZY");
    assert_eq!(request.kind(), RequestKind::Unknown);
    assert_eq!(request.route_class(), RequestRouteClass::Stateful);
}

#[test]
fn test_classify_case_insensitive_auth() {
    for line in ["authinfo user test", "Authinfo Pass test"] {
        let request = RequestContext::from_request_bytes(line.as_bytes());
        assert_eq!(request.kind(), RequestKind::AuthInfo);
        assert_eq!(request.route_class(), RequestRouteClass::Local);
    }
}

#[test]
fn test_classify_case_insensitive_stateless() {
    let cases = [
        ("quit", RequestKind::Quit, RequestRouteClass::Local),
        ("list", RequestKind::List, RequestRouteClass::Stateless),
        ("help", RequestKind::Help, RequestRouteClass::Stateless),
        ("date", RequestKind::Date, RequestRouteClass::Stateless),
        (
            "newgroups 20240101 000000",
            RequestKind::NewGroups,
            RequestRouteClass::Stateless,
        ),
        (
            "newnews * 20240101 000000",
            RequestKind::NewNews,
            RequestRouteClass::Stateless,
        ),
    ];
    // NEWGROUPS/NEWNEWS are read-only queries (RFC 3977 §7.3-7.4)
    for (line, kind, route_class) in cases {
        let request = RequestContext::from_request_bytes(line.as_bytes());
        assert_eq!(request.kind(), kind);
        assert_eq!(request.route_class(), route_class);
    }
}

#[test]
fn test_classify_case_insensitive_stateful() {
    for line in ["group alt.test", "next", "last"] {
        assert_eq!(
            RequestContext::from_request_bytes(line.as_bytes()).route_class(),
            RequestRouteClass::Stateful
        );
    }
}

#[test]
fn test_classify_case_insensitive_non_routable() {
    // POST — RFC 3977 §6.3.1: dedicated Post variant, case-insensitive
    for line in ["post", "ihave <msgid@example>"] {
        assert_eq!(
            RequestContext::from_request_bytes(line.as_bytes()).route_class(),
            RequestRouteClass::Reject
        );
    }
}
