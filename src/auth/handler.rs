//! Client authentication handling

use crate::constants::stateless_proxy::{NNTP_AUTH_ACCEPTED, NNTP_PASSWORD_REQUIRED};

/// Handles client-facing authentication interception
pub struct AuthHandler;

impl AuthHandler {
    /// Get the AUTHINFO USER response
    #[inline]
    pub fn user_response() -> &'static [u8] {
        NNTP_PASSWORD_REQUIRED
    }

    /// Get the AUTHINFO PASS response
    #[inline]
    pub fn pass_response() -> &'static [u8] {
        NNTP_AUTH_ACCEPTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_response() {
        let response = AuthHandler::user_response();
        let response_str = String::from_utf8_lossy(response);

        // Should be 381 Password required
        assert!(response_str.starts_with("381"));
        assert!(response_str.contains("Password required") || response_str.contains("password"));
        assert!(response_str.ends_with("\r\n"));
    }

    #[test]
    fn test_pass_response() {
        let response = AuthHandler::pass_response();
        let response_str = String::from_utf8_lossy(response);

        // Should be 281 Authentication accepted
        assert!(response_str.starts_with("281"));
        assert!(response_str.contains("accepted") || response_str.contains("Authentication"));
        assert!(response_str.ends_with("\r\n"));
    }

    #[test]
    fn test_responses_are_static() {
        // Verify responses are the same each time (static)
        let response1 = AuthHandler::user_response();
        let response2 = AuthHandler::user_response();
        assert_eq!(response1.as_ptr(), response2.as_ptr());

        let response3 = AuthHandler::pass_response();
        let response4 = AuthHandler::pass_response();
        assert_eq!(response3.as_ptr(), response4.as_ptr());
    }

    #[test]
    fn test_responses_are_different() {
        // User and pass responses should be different
        let user_resp = AuthHandler::user_response();
        let pass_resp = AuthHandler::pass_response();
        assert_ne!(user_resp, pass_resp);
    }

    #[test]
    fn test_responses_are_valid_utf8() {
        // Ensure responses are valid UTF-8
        let user_resp = AuthHandler::user_response();
        assert!(std::str::from_utf8(user_resp).is_ok());

        let pass_resp = AuthHandler::pass_response();
        assert!(std::str::from_utf8(pass_resp).is_ok());
    }
}
