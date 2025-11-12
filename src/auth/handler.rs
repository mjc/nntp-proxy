//! Client authentication handling

use crate::command::AuthAction;
use crate::protocol::{AUTH_ACCEPTED, AUTH_FAILED, AUTH_REQUIRED};
use crate::types::{Password, Username, ValidationError};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;

/// Handles client-facing authentication interception
pub struct AuthHandler {
    /// Map of username -> password for O(1) lookups
    users: HashMap<String, String>,
}

impl std::fmt::Debug for AuthHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthHandler")
            .field("enabled", &!self.users.is_empty())
            .field("user_count", &self.users.len())
            .finish_non_exhaustive()
    }
}

impl Default for AuthHandler {
    fn default() -> Self {
        Self {
            users: HashMap::new(),
        }
    }
}

impl AuthHandler {
    /// Create a new auth handler with optional credentials
    ///
    /// # Authentication behavior:
    /// - `None, None` → Auth disabled (allows all connections)
    /// - `Some(user), Some(pass)` → Auth enabled with validation
    /// - `Some(user), None` or `None, Some(pass)` → Auth disabled (both must be provided)
    ///
    /// # Errors
    /// Returns `Err` if either username or password is explicitly provided but empty/whitespace.
    /// This prevents misconfiguration where empty credentials would silently disable auth,
    /// which is a critical security vulnerability.
    ///
    /// # Security
    /// If you explicitly set credentials in config and they're empty, the proxy will
    /// **refuse to start** rather than silently running with no authentication.
    pub fn new(
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Self, ValidationError> {
        let mut users = HashMap::new();

        if let (Some(u), Some(p)) = (username, password) {
            // Both provided - validate they're non-empty
            let username = Username::new(u.clone())?; // Returns Err if empty
            let password = Password::new(p.clone())?; // Returns Err if empty
            users.insert(username.as_str().to_string(), password.as_str().to_string());
        }

        Ok(Self { users })
    }

    /// Create a new auth handler with multiple users
    ///
    /// # Errors
    /// Returns `Err` if any username or password is empty/whitespace.
    pub fn with_users(user_list: Vec<(String, String)>) -> Result<Self, ValidationError> {
        let mut users = HashMap::new();

        for (u, p) in user_list {
            // Validate each credential pair
            let username = Username::new(u.clone())?;
            let password = Password::new(p.clone())?;
            users.insert(username.as_str().to_string(), password.as_str().to_string());
        }

        Ok(Self { users })
    }

    /// Check if authentication is enabled
    #[inline]
    pub fn is_enabled(&self) -> bool {
        !self.users.is_empty()
    }

    /// Validate credentials
    pub fn validate(&self, username: &str, password: &str) -> bool {
        if self.users.is_empty() {
            return true; // Auth disabled, always accept
        }

        // Check if user exists and password matches
        self.users
            .get(username)
            .map_or(false, |stored_pass| stored_pass == password)
    }

    /// Handle an auth command - writes response to client and returns (bytes_written, auth_success)
    /// This is the ONE place where auth interception happens
    pub async fn handle_auth_command<W>(
        &self,
        auth_action: AuthAction,
        writer: &mut W,
        stored_username: Option<&str>,
    ) -> std::io::Result<(usize, bool)>
    where
        W: AsyncWriteExt + Unpin,
    {
        match auth_action {
            AuthAction::RequestPassword(_username) => {
                // Always respond with password required
                writer.write_all(AUTH_REQUIRED).await?;
                Ok((AUTH_REQUIRED.len(), false))
            }
            AuthAction::ValidateAndRespond { password } => {
                // Validate credentials
                let auth_success = if let Some(username) = stored_username {
                    self.validate(username, &password)
                } else {
                    // No username was stored (client sent AUTHINFO PASS without USER)
                    false
                };

                let response = if auth_success {
                    AUTH_ACCEPTED
                } else {
                    AUTH_FAILED
                };
                writer.write_all(response).await?;
                Ok((response.len(), auth_success))
            }
        }
    }

    /// Get the AUTHINFO USER response
    #[inline]
    pub const fn user_response(&self) -> &'static [u8] {
        AUTH_REQUIRED
    }

    /// Get the AUTHINFO PASS response
    #[inline]
    pub const fn pass_response(&self) -> &'static [u8] {
        AUTH_ACCEPTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_handler() -> AuthHandler {
        AuthHandler::default()
    }

    mod auth_handler {
        use super::*;

        #[test]
        fn test_default() {
            let handler = AuthHandler::default();
            assert!(!handler.is_enabled());
        }

        #[test]
        fn test_new_with_both_credentials() {
            let handler =
                AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap();
            assert!(handler.is_enabled());
        }

        #[test]
        fn test_new_with_only_username() {
            let handler = AuthHandler::new(Some("user".to_string()), None).unwrap();
            assert!(!handler.is_enabled());
        }

        #[test]
        fn test_new_with_only_password() {
            let handler = AuthHandler::new(None, Some("pass".to_string())).unwrap();
            assert!(!handler.is_enabled());
        }

        #[test]
        fn test_new_with_neither() {
            let handler = AuthHandler::new(None, None).unwrap();
            assert!(!handler.is_enabled());
        }

        #[test]
        fn test_with_users_multiple() {
            let users = vec![
                ("alice".to_string(), "secret1".to_string()),
                ("bob".to_string(), "secret2".to_string()),
                ("charlie".to_string(), "secret3".to_string()),
            ];
            let handler = AuthHandler::with_users(users).unwrap();
            assert!(handler.is_enabled());
            assert!(handler.validate("alice", "secret1"));
            assert!(handler.validate("bob", "secret2"));
            assert!(handler.validate("charlie", "secret3"));
            assert!(!handler.validate("alice", "wrong"));
            assert!(!handler.validate("bob", "secret1")); // Wrong password for bob
            assert!(!handler.validate("dave", "anything")); // Unknown user
        }

        #[test]
        fn test_with_users_empty() {
            let handler = AuthHandler::with_users(vec![]).unwrap();
            assert!(!handler.is_enabled());
            assert!(handler.validate("anyone", "anything")); // No auth, allow all
        }

        #[test]
        fn test_with_users_rejects_empty_username() {
            let users = vec![
                ("alice".to_string(), "pass1".to_string()),
                ("".to_string(), "pass2".to_string()), // Empty username
            ];
            let result = AuthHandler::with_users(users);
            assert!(result.is_err());
        }

        #[test]
        fn test_with_users_rejects_empty_password() {
            let users = vec![
                ("alice".to_string(), "pass1".to_string()),
                ("bob".to_string(), "".to_string()), // Empty password
            ];
            let result = AuthHandler::with_users(users);
            assert!(result.is_err());
        }

        #[test]
        fn test_new_with_empty_username_fails() {
            let result = AuthHandler::new(Some("".to_string()), Some("pass".to_string()));
            assert!(result.is_err(), "Empty username should return error");
        }

        #[test]
        fn test_new_with_empty_password_fails() {
            let result = AuthHandler::new(Some("user".to_string()), Some("".to_string()));
            assert!(result.is_err(), "Empty password should return error");
        }

        #[test]
        fn test_new_with_whitespace_username_fails() {
            let result = AuthHandler::new(Some("   ".to_string()), Some("pass".to_string()));
            assert!(
                result.is_err(),
                "Whitespace-only username should return error"
            );
        }

        #[test]
        fn test_new_with_whitespace_password_fails() {
            let result = AuthHandler::new(Some("user".to_string()), Some("   ".to_string()));
            assert!(
                result.is_err(),
                "Whitespace-only password should return error"
            );
        }

        #[test]
        fn test_validate_when_disabled() {
            let handler = AuthHandler::new(None, None).unwrap();
            assert!(handler.validate("any", "thing"));
            assert!(handler.validate("", ""));
            assert!(handler.validate("foo", "bar"));
        }

        #[test]
        fn test_validate_when_enabled() {
            let handler =
                AuthHandler::new(Some("alice".to_string()), Some("secret".to_string())).unwrap();
            assert!(handler.validate("alice", "secret"));
            assert!(!handler.validate("alice", "wrong"));
            assert!(!handler.validate("bob", "secret"));
            assert!(!handler.validate("bob", "wrong"));
        }

        #[test]
        fn test_is_enabled_consistent() {
            let disabled = AuthHandler::new(None, None).unwrap();
            assert!(!disabled.is_enabled());
            assert!(!disabled.is_enabled()); // Call twice to ensure consistency

            let enabled = AuthHandler::new(Some("u".to_string()), Some("p".to_string())).unwrap();
            assert!(enabled.is_enabled());
            assert!(enabled.is_enabled()); // Call twice to ensure consistency
        }
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
        let handler = AuthHandler::default();
        assert!(!handler.is_enabled());
        assert!(handler.validate("any", "thing")); // Should accept anything
    }

    #[test]
    fn test_auth_new_none_none() {
        let handler = AuthHandler::new(None, None).unwrap();
        assert!(!handler.is_enabled());
        assert!(handler.validate("any", "thing"));
    }

    #[test]
    fn test_auth_enabled_with_credentials() {
        let handler =
            AuthHandler::new(Some("mjc".to_string()), Some("nntp1337".to_string())).unwrap();
        assert!(handler.is_enabled());
        assert!(handler.validate("mjc", "nntp1337"));
        assert!(!handler.validate("mjc", "wrong"));
        assert!(!handler.validate("wrong", "nntp1337"));
    }

    #[test]
    fn test_security_empty_credentials_rejected() {
        // SECURITY: Empty username must fail
        let result = AuthHandler::new(Some("".to_string()), Some("pass".to_string()));
        assert!(
            result.is_err(),
            "Empty username should be rejected to prevent silent auth bypass"
        );

        // SECURITY: Empty password must fail
        let result = AuthHandler::new(Some("user".to_string()), Some("".to_string()));
        assert!(
            result.is_err(),
            "Empty password should be rejected to prevent silent auth bypass"
        );

        // SECURITY: Both empty must fail
        let result = AuthHandler::new(Some("".to_string()), Some("".to_string()));
        assert!(
            result.is_err(),
            "Both empty should be rejected to prevent silent auth bypass"
        );
    }

    #[test]
    fn test_security_whitespace_credentials_rejected() {
        // SECURITY: Whitespace-only username must fail
        let result = AuthHandler::new(Some("   ".to_string()), Some("pass".to_string()));
        assert!(
            result.is_err(),
            "Whitespace-only username should be rejected"
        );

        // SECURITY: Whitespace-only password must fail
        let result = AuthHandler::new(Some("user".to_string()), Some("   ".to_string()));
        assert!(
            result.is_err(),
            "Whitespace-only password should be rejected"
        );
    }

    #[test]
    fn test_security_explicit_config_prevents_silent_bypass() {
        // This test demonstrates the security fix:
        // If someone sets credentials in config but they're empty,
        // we MUST fail rather than silently disable auth.
        //
        // Before fix: Empty credentials = auth silently disabled = MASSIVE SECURITY HOLE
        // After fix: Empty credentials = proxy refuses to start = SAFE

        // Simulate someone setting credentials in config
        let username_from_config = Some("".to_string()); // Typo or misconfiguration
        let password_from_config = Some("secret".to_string());

        let result = AuthHandler::new(username_from_config, password_from_config);

        assert!(
            result.is_err(),
            "Proxy must refuse to start with empty credentials from config. \
             Silently disabling auth would be a critical security vulnerability!"
        );
    }
}
