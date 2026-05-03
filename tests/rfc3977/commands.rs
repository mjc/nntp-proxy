//! RFC 3977 Section 3.1 - Command Format Tests
//!
//! These tests verify compliance with NNTP command format requirements:
//! - Commands terminate with CRLF
//! - Commands must not exceed 512 octets
//! - Keywords are case-insensitive (but we send uppercase)
//! - Command classification is correct
//!
//! Adapted from nntp-rs RFC 3977 commands.rs tests.

use futures::executor::block_on;
use nntp_proxy::protocol;
use nntp_proxy::protocol::{RequestContext, RequestKind, RequestRouteClass};
use nntp_proxy::types::MessageId;

fn msgid(value: &str) -> MessageId<'_> {
    MessageId::from_borrowed(value).unwrap()
}

fn wire(context: &protocol::RequestContext) -> Vec<u8> {
    let mut out = Vec::with_capacity(context.request_wire_len().get());
    block_on(context.write_wire_to(&mut out)).unwrap();
    out
}

fn assert_wire(context: &RequestContext, expected: &[u8]) {
    assert_eq!(wire(context), expected);
}

fn standard_requests() -> Vec<RequestContext> {
    let id = msgid("<msgid@example>");
    vec![
        protocol::authinfo_user("user"),
        protocol::authinfo_pass("pass"),
        protocol::article_request(&id),
        protocol::body_request(&id),
        protocol::head_request(&id),
        protocol::stat_request(&id),
        protocol::date_request(),
    ]
}

fn assert_class(line: &str, kind: RequestKind, route_class: RequestRouteClass) {
    let request = RequestContext::parse(line.as_bytes());
    assert_eq!(request.kind(), kind, "{line}");
    assert_eq!(request.route_class(), route_class, "{line}");
}

// === Command Termination (RFC 3977 §3.1) ===

#[test]
fn test_all_commands_end_with_crlf() {
    // RFC 3977 §3.1: "Commands in NNTP MUST use the canonical CRLF"
    standard_requests()
        .into_iter()
        .map(|request| wire(&request))
        .for_each(|cmd| {
            assert!(
                cmd.ends_with(b"\r\n"),
                "Command does not end with CRLF: {cmd:?}"
            );
        });

    assert!(protocol::QUIT.ends_with(b"\r\n"));
}

#[test]
fn test_commands_only_one_crlf() {
    // Commands should have exactly one CRLF - prevents command injection
    standard_requests()
        .into_iter()
        .map(|request| wire(&request))
        .for_each(|cmd| {
            assert_eq!(
                cmd.windows(2).filter(|window| *window == b"\r\n").count(),
                1,
                "Command has multiple CRLFs (injection risk): {cmd:?}"
            );
        });
}

// === AUTHINFO Commands (RFC 4643) ===

#[test]
fn test_authinfo_command_format() {
    [
        (
            protocol::authinfo_user("testuser"),
            b"AUTHINFO USER testuser\r\n".as_slice(),
        ),
        (
            protocol::authinfo_user("user name"),
            b"AUTHINFO USER user name\r\n".as_slice(),
        ),
        (
            protocol::authinfo_pass("p@ss!w0rd#$%"),
            b"AUTHINFO PASS p@ss!w0rd#$%\r\n".as_slice(),
        ),
    ]
    .into_iter()
    .for_each(|(request, expected)| assert_wire(&request, expected));
}

// === ARTICLE/HEAD/BODY/STAT Commands (RFC 3977 §6.2) ===

#[test]
fn test_article_body_head_stat_commands_with_message_id() {
    let message_id = msgid("<test@example.com>");

    [
        (
            protocol::article_request(&message_id),
            b"ARTICLE <test@example.com>\r\n".as_slice(),
        ),
        (
            protocol::body_request(&message_id),
            b"BODY <test@example.com>\r\n".as_slice(),
        ),
        (
            protocol::head_request(&message_id),
            b"HEAD <test@example.com>\r\n".as_slice(),
        ),
        (
            protocol::stat_request(&message_id),
            b"STAT <test@example.com>\r\n".as_slice(),
        ),
    ]
    .into_iter()
    .for_each(|(request, expected)| assert_wire(&request, expected));
}

// === Command Constants ===

#[test]
fn test_command_constants() {
    assert_eq!(protocol::QUIT, b"QUIT\r\n");
    assert_wire(&protocol::date_request(), b"DATE\r\n");
}

// === Keywords Are Uppercase ===

#[test]
fn test_keywords_are_uppercase() {
    // RFC 3977 §3.1: Keywords are case-insensitive, convention is UPPERCASE
    let message_id = msgid("<m>");
    [
        (protocol::authinfo_user("u"), b"AUTHINFO USER ".as_slice()),
        (protocol::authinfo_pass("p"), b"AUTHINFO PASS ".as_slice()),
        (
            protocol::article_request(&message_id),
            b"ARTICLE ".as_slice(),
        ),
        (protocol::body_request(&message_id), b"BODY ".as_slice()),
        (protocol::head_request(&message_id), b"HEAD ".as_slice()),
        (protocol::stat_request(&message_id), b"STAT ".as_slice()),
        (protocol::date_request(), b"DATE".as_slice()),
    ]
    .into_iter()
    .for_each(|(request, prefix)| assert!(wire(&request).starts_with(prefix)));

    assert!(protocol::QUIT.starts_with(b"QUIT"));
}

// === Command Length (RFC 3977 §3.1) ===

#[test]
fn test_standard_commands_under_512_octets() {
    // RFC 3977 §3.1: "command line MUST NOT exceed 512 octets"
    let message_id = msgid("<typical-msgid@example.com>");
    [
        protocol::authinfo_user("typical_username"),
        protocol::authinfo_pass("typical_password"),
        protocol::article_request(&message_id),
        protocol::body_request(&message_id),
    ]
    .into_iter()
    .map(|request| String::from_utf8(wire(&request)).unwrap())
    .for_each(|cmd| {
        assert!(
            cmd.len() <= 512,
            "Command exceeds 512 octets ({} bytes): {:?}",
            cmd.len(),
            cmd
        );
    });
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
fn test_classify_commands() {
    [
        (
            "ARTICLE <test@example.com>",
            RequestKind::Article,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "BODY <test@example.com>",
            RequestKind::Body,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "HEAD <test@example.com>",
            RequestKind::Head,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "STAT <test@example.com>",
            RequestKind::Stat,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "ARTICLE 12345",
            RequestKind::Article,
            RequestRouteClass::Stateful,
        ),
        ("BODY 12345", RequestKind::Body, RequestRouteClass::Stateful),
        (
            "article <test@example.com>",
            RequestKind::Article,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "Article <test@example.com>",
            RequestKind::Article,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "ARTICLE <test@example.com>",
            RequestKind::Article,
            RequestRouteClass::ArticleByMessageId,
        ),
        (
            "AUTHINFO USER testuser",
            RequestKind::AuthInfo,
            RequestRouteClass::Local,
        ),
        (
            "AUTHINFO PASS testpass",
            RequestKind::AuthInfo,
            RequestRouteClass::Local,
        ),
        ("QUIT", RequestKind::Quit, RequestRouteClass::Local),
        ("DATE", RequestKind::Date, RequestRouteClass::Stateless),
        ("LIST", RequestKind::List, RequestRouteClass::Stateless),
        ("HELP", RequestKind::Help, RequestRouteClass::Stateless),
        (
            "CAPABILITIES",
            RequestKind::Capabilities,
            RequestRouteClass::Local,
        ),
        (
            "GROUP alt.test",
            RequestKind::Group,
            RequestRouteClass::Stateful,
        ),
        ("NEXT", RequestKind::Next, RequestRouteClass::Stateful),
        ("LAST", RequestKind::Last, RequestRouteClass::Stateful),
        (
            "XOVER 1-100",
            RequestKind::Xover,
            RequestRouteClass::Stateful,
        ),
        ("POST", RequestKind::Post, RequestRouteClass::Reject),
        (
            "IHAVE <msgid@example>",
            RequestKind::Ihave,
            RequestRouteClass::Reject,
        ),
        ("XYZZY", RequestKind::Unknown, RequestRouteClass::Stateful),
        (
            "authinfo user test",
            RequestKind::AuthInfo,
            RequestRouteClass::Local,
        ),
        (
            "Authinfo Pass test",
            RequestKind::AuthInfo,
            RequestRouteClass::Local,
        ),
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
        (
            "group alt.test",
            RequestKind::Group,
            RequestRouteClass::Stateful,
        ),
        ("next", RequestKind::Next, RequestRouteClass::Stateful),
        ("last", RequestKind::Last, RequestRouteClass::Stateful),
        ("post", RequestKind::Post, RequestRouteClass::Reject),
        (
            "ihave <msgid@example>",
            RequestKind::Ihave,
            RequestRouteClass::Reject,
        ),
    ]
    .into_iter()
    .for_each(|(line, kind, route_class)| assert_class(line, kind, route_class));

    let _ = RequestContext::parse(b"");
    let _ = RequestContext::parse(b"   ");
}
