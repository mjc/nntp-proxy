//! Common utilities shared across handler modules

use crate::auth::AuthHandler;
use crate::command::AuthAction;
use crate::types::BytesTransferred;
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Threshold for logging detailed transfer info (bytes)
/// Transfers under this size are considered "small" (test connections, etc.)
pub(super) const SMALL_TRANSFER_THRESHOLD: u64 = 500;

/// Result of handling an auth command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AuthResult {
    /// Number of bytes written to client
    pub bytes_written: BytesTransferred,
    /// Whether authentication succeeded
    pub authenticated: bool,
}

impl AuthResult {
    /// Create a new auth result
    #[inline]
    pub const fn new(bytes_written: BytesTransferred, authenticated: bool) -> Self {
        Self {
            bytes_written,
            authenticated,
        }
    }
}

/// Result of checking for QUIT command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuitStatus {
    /// QUIT command was detected and response sent (contains bytes written)
    Quit(BytesTransferred),
    /// Not a QUIT command
    Continue,
}

/// Extract message-ID from NNTP command if present
#[inline]
pub(super) fn extract_message_id(command: &str) -> Option<&str> {
    let start = command.find('<')?;
    let end = command[start..].find('>')?;
    Some(&command[start..start + end + 1])
}

/// Handle AUTHINFO command and update auth state
///
/// This function encapsulates the common auth handling logic used across
/// all routing modes. It:
/// 1. Stores username if AUTHINFO USER command
/// 2. Calls the auth handler to process the command
/// 3. Updates the authenticated flag on success
/// 4. Returns bytes written and success status
///
/// # Arguments
///
/// * `auth_handler` - The authentication handler to use
/// * `auth_action` - The parsed auth action from command classification
/// * `client_write` - Write half of client connection for responses
/// * `auth_username` - Mutable reference to Option<String> that stores username between USER and PASS
/// * `authenticated` - AtomicBool flag to set when auth succeeds
///
/// # Returns
///
/// `AuthResult` containing bytes written and whether authentication succeeded
pub(super) async fn handle_auth_command<W>(
    auth_handler: &Arc<AuthHandler>,
    auth_action: AuthAction,
    client_write: &mut W,
    auth_username: &mut Option<String>,
    authenticated: &std::sync::atomic::AtomicBool,
) -> Result<AuthResult>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // Store username if this is AUTHINFO USER
    if let AuthAction::RequestPassword(ref username) = auth_action {
        *auth_username = Some(username.clone());
    }

    // Handle auth and validate
    let (bytes, auth_success) = auth_handler
        .handle_auth_command(auth_action, client_write, auth_username.as_deref())
        .await?;

    if auth_success {
        authenticated.store(true, std::sync::atomic::Ordering::Release);
    }

    let mut bytes_written = BytesTransferred::zero();
    bytes_written.add(bytes);

    Ok(AuthResult::new(bytes_written, auth_success))
}

/// Check if command is QUIT and send closing response if so
///
/// # Returns
///
/// `QuitStatus::Quit(bytes)` if QUIT was detected and response sent
/// `QuitStatus::Continue` if not a QUIT command
pub(super) async fn handle_quit_command<W>(
    command: &str,
    client_write: &mut W,
) -> Result<QuitStatus>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if command.trim().eq_ignore_ascii_case("QUIT") {
        use crate::protocol::CONNECTION_CLOSING;

        client_write
            .write_all(CONNECTION_CLOSING)
            .await
            .inspect_err(|e| {
                tracing::debug!("Failed to write CONNECTION_CLOSING: {}", e);
            })?;

        let mut bytes = BytesTransferred::zero();
        bytes.add(CONNECTION_CLOSING.len());
        Ok(QuitStatus::Quit(bytes))
    } else {
        Ok(QuitStatus::Continue)
    }
}
