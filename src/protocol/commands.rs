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

/// Construct AUTHINFO USER command (RFC 4643 Section 2.3)
///
/// Returns a properly formatted AUTHINFO USER command with CRLF termination.
#[inline]
#[must_use]
pub fn authinfo_user(username: &str) -> String {
    format!("AUTHINFO USER {username}\r\n")
}

/// Construct AUTHINFO PASS command (RFC 4643 Section 2.4)
///
/// Returns a properly formatted AUTHINFO PASS command with CRLF termination.
#[inline]
#[must_use]
pub fn authinfo_pass(password: &str) -> String {
    format!("AUTHINFO PASS {password}\r\n")
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

    fn msgid(value: &str) -> MessageId<'_> {
        MessageId::from_borrowed(value).unwrap()
    }

    fn wire(context: &RequestContext) -> Vec<u8> {
        let mut out = Vec::with_capacity(context.wire_len());
        out.extend_from_slice(context.verb());
        if !context.args().is_empty() {
            out.push(b' ');
            out.extend_from_slice(context.args());
        }
        out.extend_from_slice(b"\r\n");
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
        assert_eq!(authinfo_user("testuser"), "AUTHINFO USER testuser\r\n");
        assert_eq!(authinfo_user(""), "AUTHINFO USER \r\n");
    }

    #[test]
    fn test_authinfo_user_special_chars() {
        assert_eq!(
            authinfo_user("user@example.com"),
            "AUTHINFO USER user@example.com\r\n"
        );
    }

    #[test]
    fn test_authinfo_user_spaces() {
        assert_eq!(
            authinfo_user("user with spaces"),
            "AUTHINFO USER user with spaces\r\n"
        );
    }

    #[test]
    fn test_authinfo_pass() {
        assert_eq!(authinfo_pass("secret"), "AUTHINFO PASS secret\r\n");
        assert_eq!(authinfo_pass(""), "AUTHINFO PASS \r\n");
    }

    #[test]
    fn test_authinfo_pass_special_chars() {
        assert_eq!(
            authinfo_pass("p@ssw0rd!#$"),
            "AUTHINFO PASS p@ssw0rd!#$\r\n"
        );
    }

    #[test]
    fn test_authinfo_pass_spaces() {
        assert_eq!(authinfo_pass("pass word"), "AUTHINFO PASS pass word\r\n");
    }

    #[test]
    fn test_authinfo_crlf_termination() {
        assert!(authinfo_user("user").ends_with("\r\n"));
        assert!(authinfo_pass("pass").ends_with("\r\n"));
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
        assert!(authinfo_user("test").ends_with("\r\n"));
        assert!(authinfo_pass("test").ends_with("\r\n"));
        assert!(wire(&article_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
        assert!(wire(&body_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
        assert!(wire(&head_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
        assert!(wire(&stat_request(&msgid("<test@example.com>"))).ends_with(b"\r\n"));
    }
}
