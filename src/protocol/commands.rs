//! NNTP command construction helpers
//!
//! This module provides functions for constructing well-formed NNTP commands
//! according to RFC 3977 and RFC 4643.

/// QUIT command (RFC 3977 Section 5.4)
pub const QUIT: &[u8] = b"QUIT\r\n";

/// COMPRESS DEFLATE command (RFC 8054 §2.2)
pub const COMPRESS_DEFLATE: &[u8] = b"COMPRESS DEFLATE\r\n";

/// DATE command (RFC 3977 Section 7.1)
///
/// The DATE command returns the server's current date and time in UTC.
pub const DATE: &[u8] = b"DATE\r\n";

/// Construct AUTHINFO USER command (RFC 4643 Section 2.3)
///
/// Returns a properly formatted AUTHINFO USER command with CRLF termination.
#[inline]
pub fn authinfo_user(username: &str) -> String {
    format!("AUTHINFO USER {}\r\n", username)
}

/// Construct AUTHINFO PASS command (RFC 4643 Section 2.4)
///
/// Returns a properly formatted AUTHINFO PASS command with CRLF termination.
#[inline]
pub fn authinfo_pass(password: &str) -> String {
    format!("AUTHINFO PASS {}\r\n", password)
}

/// Construct ARTICLE command with message-ID (RFC 3977 Section 6.2.1)
///
/// Returns a properly formatted ARTICLE command for retrieving an article by message-ID.
#[inline]
pub fn article_by_msgid(msgid: &str) -> String {
    format!("ARTICLE {}\r\n", msgid)
}

/// Construct BODY command with message-ID (RFC 3977 Section 6.2.3)
///
/// Returns a properly formatted BODY command for retrieving article body by message-ID.
#[inline]
pub fn body_by_msgid(msgid: &str) -> String {
    format!("BODY {}\r\n", msgid)
}

/// Construct HEAD command with message-ID (RFC 3977 Section 6.2.2)
///
/// Returns a properly formatted HEAD command for retrieving article headers by message-ID.
#[inline]
pub fn head_by_msgid(msgid: &str) -> String {
    format!("HEAD {}\r\n", msgid)
}

/// Construct STAT command with message-ID (RFC 3977 Section 6.2.4)
///
/// Returns a properly formatted STAT command for checking article existence by message-ID.
#[inline]
pub fn stat_by_msgid(msgid: &str) -> String {
    format!("STAT {}\r\n", msgid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quit_command() {
        assert_eq!(QUIT, b"QUIT\r\n");
    }

    #[test]
    fn test_date_command() {
        assert_eq!(DATE, b"DATE\r\n");
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
    fn test_article_by_msgid() {
        assert_eq!(
            article_by_msgid("<test@example.com>"),
            "ARTICLE <test@example.com>\r\n"
        );
        assert_eq!(
            article_by_msgid("<msg123@news.server.com>"),
            "ARTICLE <msg123@news.server.com>\r\n"
        );
    }

    #[test]
    fn test_body_by_msgid() {
        assert_eq!(
            body_by_msgid("<test@example.com>"),
            "BODY <test@example.com>\r\n"
        );
    }

    #[test]
    fn test_head_by_msgid() {
        assert_eq!(
            head_by_msgid("<test@example.com>"),
            "HEAD <test@example.com>\r\n"
        );
    }

    #[test]
    fn test_stat_by_msgid() {
        assert_eq!(
            stat_by_msgid("<test@example.com>"),
            "STAT <test@example.com>\r\n"
        );
    }

    #[test]
    fn test_commands_end_with_crlf() {
        assert!(authinfo_user("test").ends_with("\r\n"));
        assert!(authinfo_pass("test").ends_with("\r\n"));
        assert!(article_by_msgid("<test@example.com>").ends_with("\r\n"));
        assert!(body_by_msgid("<test@example.com>").ends_with("\r\n"));
        assert!(head_by_msgid("<test@example.com>").ends_with("\r\n"));
        assert!(stat_by_msgid("<test@example.com>").ends_with("\r\n"));
    }
}
