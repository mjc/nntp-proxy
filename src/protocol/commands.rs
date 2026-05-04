//! NNTP command construction helpers
//!
//! This module provides functions for constructing well-formed NNTP commands
//! according to RFC 3977 and RFC 4643.

use super::RequestContext;
use crate::types::MessageId;

/// QUIT command (RFC 3977 Section 5.4)
pub const QUIT: &[u8] = b"QUIT\r\n";

/// COMPRESS DEFLATE command (RFC 8054 §2.2)
pub const COMPRESS_DEFLATE: &[u8] = b"COMPRESS DEFLATE\r\n";

/// Construct AUTHINFO USER request (RFC 4643 Section 2.3)
///
/// Returns a typed request context with verb `AUTHINFO` and args `USER <username>`.
#[inline]
#[must_use]
pub fn authinfo_user(username: &str) -> RequestContext {
    RequestContext::from_verb_arg_slices(b"AUTHINFO", &[b"USER ", username.as_bytes()])
}

/// Construct AUTHINFO PASS request (RFC 4643 Section 2.4)
///
/// Returns a typed request context with verb `AUTHINFO` and args `PASS <password>`.
#[inline]
#[must_use]
pub fn authinfo_pass(password: &str) -> RequestContext {
    RequestContext::from_verb_arg_slices(b"AUTHINFO", &[b"PASS ", password.as_bytes()])
}

#[inline]
#[must_use]
pub fn article_request(msgid: &MessageId<'_>) -> RequestContext {
    RequestContext::from_verb_args(b"ARTICLE", msgid.as_str().as_bytes())
}

#[inline]
#[must_use]
pub fn body_request(msgid: &MessageId<'_>) -> RequestContext {
    RequestContext::from_verb_args(b"BODY", msgid.as_str().as_bytes())
}

#[inline]
#[must_use]
pub fn head_request(msgid: &MessageId<'_>) -> RequestContext {
    RequestContext::from_verb_args(b"HEAD", msgid.as_str().as_bytes())
}

#[inline]
#[must_use]
pub fn stat_request(msgid: &MessageId<'_>) -> RequestContext {
    RequestContext::from_verb_args(b"STAT", msgid.as_str().as_bytes())
}

#[inline]
#[must_use]
pub fn date_request() -> RequestContext {
    RequestContext::from_verb_args(b"DATE", b"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    fn msgid(value: &str) -> MessageId<'_> {
        MessageId::from_borrowed(value).unwrap()
    }

    fn wire(context: &RequestContext) -> Vec<u8> {
        let mut out = Vec::with_capacity(context.request_wire_len().get());
        block_on(context.write_wire_to(&mut out)).unwrap();
        out
    }

    #[test]
    fn test_quit_command() {
        assert_eq!(QUIT, b"QUIT\r\n");
    }

    #[test]
    fn test_date_request() {
        assert_eq!(wire(&date_request()), b"DATE\r\n");
    }

    #[test]
    fn test_authinfo_user() {
        assert_eq!(
            wire(&authinfo_user("testuser")),
            b"AUTHINFO USER testuser\r\n"
        );
        assert_eq!(wire(&authinfo_user("")), b"AUTHINFO USER \r\n");
        assert_eq!(
            authinfo_user("testuser").kind(),
            crate::protocol::RequestKind::AuthInfo
        );
    }

    #[test]
    fn test_authinfo_user_special_chars() {
        assert_eq!(
            wire(&authinfo_user("user@example.com")),
            b"AUTHINFO USER user@example.com\r\n"
        );
    }

    #[test]
    fn test_authinfo_user_spaces() {
        assert_eq!(
            wire(&authinfo_user("user with spaces")),
            b"AUTHINFO USER user with spaces\r\n"
        );
    }

    #[test]
    fn test_authinfo_pass() {
        assert_eq!(wire(&authinfo_pass("secret")), b"AUTHINFO PASS secret\r\n");
        assert_eq!(wire(&authinfo_pass("")), b"AUTHINFO PASS \r\n");
        assert_eq!(
            authinfo_pass("secret").kind(),
            crate::protocol::RequestKind::AuthInfo
        );
    }

    #[test]
    fn test_authinfo_pass_special_chars() {
        assert_eq!(
            wire(&authinfo_pass("p@ssw0rd!#$")),
            b"AUTHINFO PASS p@ssw0rd!#$\r\n"
        );
    }

    #[test]
    fn test_authinfo_pass_spaces() {
        assert_eq!(
            wire(&authinfo_pass("pass word")),
            b"AUTHINFO PASS pass word\r\n"
        );
    }

    #[test]
    fn test_authinfo_crlf_termination() {
        assert!(wire(&authinfo_user("user")).ends_with(b"\r\n"));
        assert!(wire(&authinfo_pass("pass")).ends_with(b"\r\n"));
    }

    #[test]
    fn test_article_request() {
        assert_eq!(
            wire(&article_request(&msgid("<test@example.com>"))),
            b"ARTICLE <test@example.com>\r\n"
        );
        assert_eq!(
            wire(&article_request(&msgid("<msg123@news.server.com>"))),
            b"ARTICLE <msg123@news.server.com>\r\n"
        );
    }

    #[test]
    fn test_body_request() {
        assert_eq!(
            wire(&body_request(&msgid("<test@example.com>"))),
            b"BODY <test@example.com>\r\n"
        );
    }

    #[test]
    fn test_head_request() {
        assert_eq!(
            wire(&head_request(&msgid("<test@example.com>"))),
            b"HEAD <test@example.com>\r\n"
        );
    }

    #[test]
    fn test_stat_request() {
        assert_eq!(
            wire(&stat_request(&msgid("<test@example.com>"))),
            b"STAT <test@example.com>\r\n"
        );
    }

    #[test]
    fn test_commands_end_with_crlf() {
        assert!(wire(&authinfo_user("test")).ends_with(b"\r\n"));
        assert!(wire(&authinfo_pass("test")).ends_with(b"\r\n"));
        assert!(wire(&article_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
        assert!(wire(&body_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
        assert!(wire(&head_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
        assert!(wire(&stat_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
    }
}
