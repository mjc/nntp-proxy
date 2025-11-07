//! Client authentication handling

use crate::command::AuthAction;
use crate::protocol::{AUTH_ACCEPTED, AUTH_REQUIRED};
use tokio::io::AsyncWriteExt;

/// Handles client-facing authentication interception
pub struct AuthHandler {
    username: Option<String>,
    password: Option<String>,
}

impl std::fmt::Debug for AuthHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthHandler")
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl AuthHandler {
    /// Create a new auth handler with optional credentials
    pub fn new(username: Option<String>, password: Option<String>) -> Self {
        Self { username, password }
    }

    /// Check if authentication is enabled
    pub fn is_enabled(&self) -> bool {
        self.username.is_some() && self.password.is_some()
    }

    /// Validate credentials
    pub fn validate(&self, username: &str, password: &str) -> bool {
        if !self.is_enabled() {
            return true; // Auth disabled, always accept
        }

        self.username.as_deref() == Some(username) && self.password.as_deref() == Some(password)
    }

    /// Handle an auth command - writes response to client
    /// This is the ONE place where auth interception happens
    pub async fn handle_auth_command<W>(
        &self,
        auth_action: AuthAction,
        writer: &mut W,
    ) -> std::io::Result<usize>
    where
        W: AsyncWriteExt + Unpin,
    {
        let response = match auth_action {
            AuthAction::RequestPassword => AUTH_REQUIRED,
            AuthAction::AcceptAuth => AUTH_ACCEPTED,
        };
        writer.write_all(response).await?;
        Ok(response.len())
    }

    /// Get the AUTHINFO USER response
    #[inline]
    pub fn user_response(&self) -> &'static [u8] {
        AUTH_REQUIRED
    }

    /// Get the AUTHINFO PASS response
    #[inline]
    pub fn pass_response(&self) -> &'static [u8] {
        AUTH_ACCEPTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_handler() -> AuthHandler {
        AuthHandler::new(None, None)
    }

    #[test]
    fn test_user_response() {
        let handler = test_handler();
        let response = handler.user_response();
        let response_str = String::from_utf8_lossy(response);

        // Should be 381 Password required
        assert!(response_str.starts_with("381"));
        assert!(response_str.contains("Password required") || response_str.contains("password"));
        assert!(response_str.ends_with("\r\n"));
    }

    #[test]
    fn test_pass_response() {
        let handler = test_handler();
        let response = handler.pass_response();
        let response_str = String::from_utf8_lossy(response);

        // Should be 281 Authentication accepted
        assert!(response_str.starts_with("281"));
        assert!(response_str.contains("accepted") || response_str.contains("Authentication"));
        assert!(response_str.ends_with("\r\n"));
    }

    #[test]
    fn test_responses_are_static() {
        // Verify responses are the same each time (static)
        let handler = test_handler();
        let response1 = handler.user_response();
        let response2 = handler.user_response();
        assert_eq!(response1.as_ptr(), response2.as_ptr());

        let response3 = handler.pass_response();
        let response4 = handler.pass_response();
        assert_eq!(response3.as_ptr(), response4.as_ptr());
    }

    #[test]
    fn test_responses_are_different() {
        // User and pass responses should be different
        let handler = test_handler();
        let user_resp = handler.user_response();
        let pass_resp = handler.pass_response();
        assert_ne!(user_resp, pass_resp);
    }

    #[test]
    fn test_responses_are_valid_utf8() {
        // Ensure responses are valid UTF-8
        let handler = test_handler();
        let user_resp = handler.user_response();
        assert!(std::str::from_utf8(user_resp).is_ok());

        let pass_resp = handler.pass_response();
        assert!(std::str::from_utf8(pass_resp).is_ok());
    }

    #[test]
    fn test_auth_disabled_by_default() {
        let handler = AuthHandler::new(None, None);
        assert!(!handler.is_enabled());
        assert!(handler.validate("any", "thing")); // Should accept anything
    }

    #[test]
    fn test_auth_enabled_with_credentials() {
        let handler = AuthHandler::new(Some("mjc".to_string()), Some("nntp1337".to_string()));
        assert!(handler.is_enabled());
        assert!(handler.validate("mjc", "nntp1337"));
        assert!(!handler.validate("mjc", "wrong"));
        assert!(!handler.validate("wrong", "nntp1337"));
    }
}
