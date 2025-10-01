//! Client authentication handling

use crate::protocol::{NNTP_AUTH_ACCEPTED, NNTP_PASSWORD_REQUIRED};

/// Handles client-facing authentication interception
pub struct AuthHandler;

impl AuthHandler {
    /// Get the response for AUTHINFO USER command
    pub fn user_response() -> &'static [u8] {
        NNTP_PASSWORD_REQUIRED
    }

    /// Get the response for AUTHINFO PASS command
    pub fn pass_response() -> &'static [u8] {
        NNTP_AUTH_ACCEPTED
    }
}
