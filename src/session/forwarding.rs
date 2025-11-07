//! Centralized command action handling for session handlers.
//!
//! This module eliminates duplication by providing a single place where
//! CommandAction::InterceptAuth and CommandAction::Reject are handled.
//! Each session handler can customize ForwardStateless behavior while
//! sharing the common auth/reject logic.

use crate::auth::AuthHandler;
use crate::command::CommandAction;
use crate::protocol::COMMAND_NOT_SUPPORTED_STATELESS;
use std::io::Result;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tracing::warn;

/// Handle authentication and rejection commands, returning None for ForwardStateless.
///
/// This centralizes the common command handling logic:
/// - InterceptAuth: Calls AuthHandler and returns bytes written
/// - Reject: Writes error message and returns bytes written  
/// - ForwardStateless: Returns None so caller can handle forwarding
///
/// # Example
///
/// ```no_run
/// use nntp_proxy::session::forwarding::handle_intercepted_command;
/// use nntp_proxy::command::CommandHandler;
///
/// # async fn example() -> std::io::Result<()> {
/// # let mut client_write = tokio::io::sink();
/// # let auth_handler = nntp_proxy::auth::AuthHandler::new(None, None);
/// # let client_addr = "127.0.0.1:8080".parse().unwrap();
/// # let command = "LIST\r\n";
/// let action = CommandHandler::handle_command(command);
/// match handle_intercepted_command(action, command, &mut client_write, &auth_handler, &client_addr).await? {
///     Some(bytes) => {
///         // Auth or reject handled, track bytes
///         println!("Wrote {} bytes", bytes);
///     }
///     None => {
///         // Forward to backend (handler-specific logic)
///         // backend.write_all(command.as_bytes()).await?;
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub async fn handle_intercepted_command<W>(
    action: CommandAction,
    command: &str,
    client_write: &mut W,
    auth_handler: &AuthHandler,
    client_addr: &SocketAddr,
) -> Result<Option<usize>>
where
    W: AsyncWriteExt + Unpin,
{
    match action {
        CommandAction::InterceptAuth(auth_action) => {
            let bytes = auth_handler
                .handle_auth_command(auth_action, client_write)
                .await?;
            Ok(Some(bytes))
        }

        CommandAction::Reject(reason) => {
            warn!(
                "Rejecting command from client {}: {} ({})",
                client_addr,
                command.trim(),
                reason
            );
            client_write
                .write_all(COMMAND_NOT_SUPPORTED_STATELESS)
                .await?;
            Ok(Some(COMMAND_NOT_SUPPORTED_STATELESS.len()))
        }

        CommandAction::ForwardStateless => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandHandler;

    #[tokio::test]
    async fn test_forward_action_returns_none() {
        let mut output = Vec::new();
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let auth = AuthHandler::new(None, None);

        let action = CommandAction::ForwardStateless;
        let result = handle_intercepted_command(action, "LIST\r\n", &mut output, &auth, &addr)
            .await
            .unwrap();

        assert!(result.is_none());
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn test_reject_action_returns_bytes() {
        let mut output = Vec::new();
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let auth = AuthHandler::new(None, None);

        let action = CommandAction::Reject("test reason");
        let result = handle_intercepted_command(action, "XOVER\r\n", &mut output, &auth, &addr)
            .await
            .unwrap();

        assert_eq!(result, Some(COMMAND_NOT_SUPPORTED_STATELESS.len()));
        assert_eq!(output, COMMAND_NOT_SUPPORTED_STATELESS);
    }

    #[tokio::test]
    async fn test_auth_action_returns_bytes() {
        let mut output = Vec::new();
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let auth = AuthHandler::new(Some("testuser".to_string()), Some("testpass".to_string()));

        let command = "AUTHINFO USER testuser\r\n";
        let action = CommandHandler::handle_command(command);

        let result = handle_intercepted_command(action, command, &mut output, &auth, &addr)
            .await
            .unwrap();

        assert!(result.is_some());
        assert!(result.unwrap() > 0);
        assert!(!output.is_empty());
    }
}
