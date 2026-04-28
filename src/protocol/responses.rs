//! NNTP response message constants and construction helpers
//!
//! This module provides pre-defined NNTP response messages and helpers
//! for constructing responses according to RFC 3977.

/// Line ending: "\r\n"
pub const CRLF: &[u8] = b"\r\n";

/// Terminator tail size for spanning terminator detection
pub const TERMINATOR_TAIL_SIZE: usize = 4;

/// Minimum response length (3-digit code + CRLF)
pub const MIN_RESPONSE_LENGTH: usize = 5;

// Authentication responses (RFC 4643)

/// Authentication required response (381)
pub const AUTH_REQUIRED: &[u8] = b"381 Password required\r\n";

/// Authentication accepted response (281)
pub const AUTH_ACCEPTED: &[u8] = b"281 Authentication accepted\r\n";

/// Authentication failed response (481)
pub const AUTH_FAILED: &[u8] = b"481 Authentication failed\r\n";

/// Authentication required for this command (480)
pub const AUTH_REQUIRED_FOR_COMMAND: &[u8] = b"480 Authentication required\r\n";

/// Authentication commands issued out of sequence (482)
///
/// Per [RFC 4643 §2.3.2](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3.2),
/// AUTHINFO PASS without a prior AUTHINFO USER must return 482.
pub const AUTH_OUT_OF_SEQUENCE: &[u8] = b"482 Authentication commands issued out of sequence\r\n";

/// Already authenticated (502)
///
/// Per [RFC 4643 §2.2](https://datatracker.ietf.org/doc/html/rfc4643#section-2.2),
/// any AUTHINFO command after successful authentication must return 502.
pub const AUTH_ALREADY_AUTHENTICATED: &[u8] = b"502 Already authenticated\r\n";

/// Unrecognized AUTHINFO subcommand (501)
///
/// Per [RFC 4643 §2.3.1](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3.1),
/// AUTHINFO with a subcommand the server does not support must return 501.
pub const AUTH_UNKNOWN_SUBCOMMAND: &[u8] = b"501 Syntax error in command\r\n";

// Standard responses

/// Proxy greeting for per-command routing mode (200)
pub const PROXY_GREETING_PCR: &[u8] = b"200 NNTP Proxy Ready (Per-Command Routing)\r\n";

/// Connection closing response (205)
pub const CONNECTION_CLOSING: &[u8] = b"205 Connection closing\r\n";

/// Goodbye message (205) - common in tests
pub const GOODBYE: &[u8] = b"205 Goodbye\r\n";

// Error responses

/// Posting not permitted (440)
///
/// Per [RFC 3977 §6.3.1](https://datatracker.ietf.org/doc/html/rfc3977#section-6.3.1),
/// servers that do not permit posting MUST return 440.
pub const POSTING_NOT_PERMITTED: &[u8] = b"440 Posting not permitted\r\n";

/// Backend error response (503)
pub const BACKEND_ERROR: &[u8] = b"503 Backend error\r\n";

/// Backend server unavailable response (400)
pub const BACKEND_UNAVAILABLE: &[u8] = b"400 Backend server unavailable\r\n";

/// No such article response (430)
pub const NO_SUCH_ARTICLE: &[u8] = b"430 No such article\r\n";

// Capabilities responses (RFC 3977 §5.2 + RFC 4643 §3.2)

/// Synthetic CAPABILITIES response without AUTHINFO (authenticated or auth disabled)
///
/// Per [RFC 4643 §3.2](https://datatracker.ietf.org/doc/html/rfc4643#section-3.2),
/// AUTHINFO MUST NOT be advertised after successful authentication.
pub const CAPABILITIES_WITHOUT_AUTHINFO: &[u8] =
    b"101 Capability list:\r\nVERSION 2\r\nREADER\r\nOVER\r\nHDR\r\n.\r\n";

/// Synthetic CAPABILITIES response with AUTHINFO (auth required, not yet authenticated)
///
/// Per [RFC 4643 §3.2](https://datatracker.ietf.org/doc/html/rfc4643#section-3.2),
/// AUTHINFO SHOULD be advertised before authentication when the server provides it.
pub const CAPABILITIES_WITH_AUTHINFO: &[u8] =
    b"101 Capability list:\r\nVERSION 2\r\nREADER\r\nAUTHINFO USER PASS\r\nOVER\r\nHDR\r\n.\r\n";

// Response construction helpers

/// Construct a greeting response (200)
///
/// # Examples
/// ```
/// use nntp_proxy::protocol::greeting;
///
/// let msg = greeting("news.example.com ready");
/// assert_eq!(msg, "200 news.example.com ready\r\n");
/// ```
#[inline]
#[must_use]
pub fn greeting(message: &str) -> String {
    format!("200 {message}\r\n")
}

/// Construct a read-only greeting response (201)
#[inline]
#[must_use]
pub fn greeting_readonly(message: &str) -> String {
    format!("201 {message}\r\n")
}

/// Construct a generic OK response (200)
#[inline]
#[must_use]
pub fn ok_response(message: &str) -> String {
    format!("200 {message}\r\n")
}

/// Construct a generic error response with custom code and message
///
/// # Examples
/// ```
/// use nntp_proxy::protocol::error_response;
///
/// let msg = error_response(430, "No such article");
/// assert_eq!(msg, "430 No such article\r\n");
/// ```
#[inline]
#[must_use]
pub fn error_response(code: u16, message: &str) -> String {
    format!("{code} {message}\r\n")
}

/// Construct a response with custom status code and message
#[inline]
#[must_use]
pub fn response(code: u16, message: &str) -> String {
    format!("{code} {message}\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(CRLF, b"\r\n");
        assert_eq!(TERMINATOR_TAIL_SIZE, 4);
    }

    #[test]
    fn test_greeting() {
        assert_eq!(greeting("Ready"), "200 Ready\r\n");
        assert_eq!(
            greeting("news.example.com ready"),
            "200 news.example.com ready\r\n"
        );
        assert_eq!(greeting_readonly("Read only"), "201 Read only\r\n");
    }

    #[test]
    fn test_ok_response() {
        assert_eq!(ok_response("OK"), "200 OK\r\n");
        assert_eq!(ok_response("Command accepted"), "200 Command accepted\r\n");
    }

    #[test]
    fn test_error_response() {
        assert_eq!(
            error_response(430, "No such article"),
            "430 No such article\r\n"
        );
        assert_eq!(
            error_response(500, "Command not recognized"),
            "500 Command not recognized\r\n"
        );
    }

    #[test]
    fn test_response() {
        assert_eq!(
            response(215, "Newsgroups follow"),
            "215 Newsgroups follow\r\n"
        );
        assert_eq!(
            response(381, "Password required"),
            "381 Password required\r\n"
        );
    }

    #[test]
    fn test_auth_constants() {
        assert_eq!(AUTH_REQUIRED, b"381 Password required\r\n");
        assert_eq!(AUTH_ACCEPTED, b"281 Authentication accepted\r\n");
        assert_eq!(AUTH_FAILED, b"481 Authentication failed\r\n");
        assert_eq!(
            AUTH_REQUIRED_FOR_COMMAND,
            b"480 Authentication required\r\n"
        );
    }

    #[test]
    fn test_standard_responses() {
        assert!(PROXY_GREETING_PCR.starts_with(b"200"));
        assert!(CONNECTION_CLOSING.starts_with(b"205"));
        assert!(GOODBYE.starts_with(b"205"));
    }

    #[test]
    fn test_error_constants() {
        assert!(POSTING_NOT_PERMITTED.starts_with(b"440"));
        assert!(BACKEND_ERROR.starts_with(b"503"));
        assert!(BACKEND_UNAVAILABLE.starts_with(b"400"));
    }

    #[test]
    fn test_capabilities_constants() {
        assert!(CAPABILITIES_WITHOUT_AUTHINFO.starts_with(b"101"));
        assert!(CAPABILITIES_WITH_AUTHINFO.starts_with(b"101"));
        // AUTHINFO present only in the pre-auth variant
        assert!(
            CAPABILITIES_WITH_AUTHINFO
                .windows(8)
                .any(|w| w == b"AUTHINFO")
        );
        assert!(
            !CAPABILITIES_WITHOUT_AUTHINFO
                .windows(8)
                .any(|w| w == b"AUTHINFO")
        );
        assert!(CAPABILITIES_WITH_AUTHINFO.ends_with(b".\r\n"));
        assert!(CAPABILITIES_WITHOUT_AUTHINFO.ends_with(b".\r\n"));
    }

    #[test]
    fn test_all_responses_end_with_crlf() {
        assert!(AUTH_REQUIRED.ends_with(CRLF));
        assert!(AUTH_ACCEPTED.ends_with(CRLF));
        assert!(AUTH_FAILED.ends_with(CRLF));
        assert!(AUTH_REQUIRED_FOR_COMMAND.ends_with(CRLF));
        assert!(PROXY_GREETING_PCR.ends_with(CRLF));
        assert!(CONNECTION_CLOSING.ends_with(CRLF));
        assert!(GOODBYE.ends_with(CRLF));
        assert!(POSTING_NOT_PERMITTED.ends_with(CRLF));
        assert!(BACKEND_ERROR.ends_with(CRLF));
        assert!(BACKEND_UNAVAILABLE.ends_with(CRLF));
    }
}
