//! Common utilities shared across handler modules

use crate::auth::AuthHandler;
use crate::command::AuthAction;
use crate::types::BackendToClientBytes;

use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Result of handling an auth command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthResult {
    /// Authentication succeeded
    Authenticated(BackendToClientBytes),
    /// Authentication failed or not required yet
    NotAuthenticated(BackendToClientBytes),
}

/// Result of checking for QUIT command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitStatus {
    /// QUIT command was detected and response sent (contains bytes written)
    Quit(BackendToClientBytes),
    /// Not a QUIT command
    Continue,
}

/// Extract message-ID from NNTP command if present
#[inline]
pub(crate) fn extract_message_id(command: &str) -> Option<&str> {
    let start = command.find('<')?;
    let end = command[start..].find('>')?;
    Some(&command[start..start + end + 1])
}

/// Handle AUTHINFO command and update auth state
pub(crate) async fn handle_auth_command<W>(
    auth_handler: &Arc<AuthHandler>,
    auth_action: AuthAction,
    client_write: &mut W,
    auth_username: &mut Option<String>,
    auth_state: &crate::session::AuthState,
) -> Result<AuthResult>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if let AuthAction::RequestPassword(ref username) = auth_action {
        *auth_username = Some(username.clone());
    }

    let (bytes, auth_success) = auth_handler
        .handle_auth_command(auth_action, client_write, auth_username.as_deref())
        .await?;

    if auth_success && let Some(ref username) = *auth_username {
        auth_state.mark_authenticated(username.clone());
    }

    let bytes_written = BackendToClientBytes::new(bytes as u64);

    Ok(if auth_success {
        AuthResult::Authenticated(bytes_written)
    } else {
        AuthResult::NotAuthenticated(bytes_written)
    })
}

/// Check if command is QUIT and send closing response
pub(crate) async fn handle_quit_command<W>(
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

        let bytes = BackendToClientBytes::new(CONNECTION_CLOSING.len() as u64);
        Ok(QuitStatus::Quit(bytes))
    } else {
        Ok(QuitStatus::Continue)
    }
}

/// Handle successful authentication with all side effects
///
/// Sets username, records connection stats, updates metrics
pub(crate) fn on_authentication_success(
    client_addr: impl std::fmt::Display,
    username: Option<String>,
    routing_mode: &crate::config::RoutingMode,
    metrics: &Option<crate::metrics::MetricsCollector>,
    connection_stats: Option<&crate::metrics::ConnectionStatsAggregator>,
    set_username_fn: impl FnOnce(Option<String>),
) {
    use tracing::debug;

    debug!("Client {} authenticated as: {:?}", client_addr, username);

    // Store username in session
    set_username_fn(username.clone());

    // Record connection for aggregation (after auth so we have username)
    if let Some(stats) = connection_stats {
        stats.record_connection(
            username.as_deref(),
            &routing_mode.to_string().to_lowercase(),
        );
    }

    // Track user connection in metrics
    if let Some(m) = metrics {
        m.user_connection_opened(username.as_deref());
        debug!(
            "Client {} opened connection for user: {:?}",
            client_addr, username
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_message_id_valid() {
        let cmd = "ARTICLE <test@example.com>";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<test@example.com>"));
    }

    #[test]
    fn test_extract_message_id_with_extra_text() {
        let cmd = "ARTICLE <msg@server.com> extra stuff";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<msg@server.com>"));
    }

    #[test]
    fn test_extract_message_id_no_brackets() {
        let cmd = "ARTICLE 123";
        let msgid = extract_message_id(cmd);
        assert!(msgid.is_none());
    }

    #[test]
    fn test_extract_message_id_empty_brackets() {
        let cmd = "ARTICLE <>";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<>"));
    }

    #[test]
    fn test_extract_message_id_incomplete_open() {
        let cmd = "ARTICLE <incomplete";
        let msgid = extract_message_id(cmd);
        assert!(msgid.is_none());
    }

    #[test]
    fn test_extract_message_id_incomplete_close() {
        let cmd = "ARTICLE incomplete>";
        let msgid = extract_message_id(cmd);
        assert!(msgid.is_none());
    }

    #[test]
    fn test_extract_message_id_multiple_brackets() {
        let cmd = "ARTICLE <first@example.com> <second@example.com>";
        let msgid = extract_message_id(cmd);
        // Should return first message-ID
        assert_eq!(msgid, Some("<first@example.com>"));
    }

    #[test]
    fn test_extract_message_id_lowercase_command() {
        let cmd = "article <test@example.com>";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<test@example.com>"));
    }

    #[test]
    fn test_extract_message_id_head_command() {
        let cmd = "HEAD <msg@example.com>";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<msg@example.com>"));
    }

    #[test]
    fn test_extract_message_id_stat_command() {
        let cmd = "STAT <article@news.server>";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<article@news.server>"));
    }

    #[test]
    fn test_extract_message_id_complex() {
        let cmd = "ARTICLE <CAFEBaBe_12345$@news.example.com>";
        let msgid = extract_message_id(cmd);
        assert_eq!(msgid, Some("<CAFEBaBe_12345$@news.example.com>"));
    }

    #[test]
    fn test_auth_result_equality() {
        let auth1 = AuthResult::Authenticated(BackendToClientBytes::new(100));
        let auth2 = AuthResult::Authenticated(BackendToClientBytes::new(100));
        let not_auth = AuthResult::NotAuthenticated(BackendToClientBytes::new(100));

        assert_eq!(auth1, auth2);
        assert_ne!(auth1, not_auth);
    }

    #[test]
    fn test_quit_status_equality() {
        let quit1 = QuitStatus::Quit(BackendToClientBytes::new(50));
        let quit2 = QuitStatus::Quit(BackendToClientBytes::new(50));
        let cont = QuitStatus::Continue;

        assert_eq!(quit1, quit2);
        assert_ne!(quit1, cont);
    }
}

// ============================================================================
// Session Loop Helpers
// ============================================================================

/// Result of handling an authentication command in a session loop
#[derive(Debug)]
pub enum AuthHandlerResult {
    /// Authentication succeeded, session can continue
    Authenticated {
        bytes_written: u64,
        skip_further_checks: bool,
    },
    /// Authentication required but not yet complete
    NotAuthenticated { bytes_written: u64 },
    /// Command rejected
    Rejected { bytes_written: u64 },
}

/// Handle authentication logic for a command in a stateful session
///
/// This encapsulates the common pattern of:
/// 1. Checking if authenticated
/// 2. Handling auth commands
/// 3. Rejecting non-auth commands when not authenticated
#[allow(clippy::too_many_arguments)]
pub async fn handle_stateful_auth_check<W>(
    command: &str,
    client_write: &mut W,
    auth_username: &mut Option<String>,
    auth_handler: &std::sync::Arc<crate::auth::AuthHandler>,
    auth_state: &crate::session::AuthState,
    routing_mode: &crate::config::RoutingMode,
    metrics: &Option<crate::metrics::MetricsCollector>,
    connection_stats: Option<&crate::metrics::ConnectionStatsAggregator>,
    client_addr: impl std::fmt::Display + Clone,
    set_username_fn: impl FnOnce(Option<String>),
) -> anyhow::Result<AuthHandlerResult>
where
    W: AsyncWriteExt + Unpin,
{
    use crate::command::{CommandAction, CommandHandler};

    let action = CommandHandler::classify(command);
    match action {
        CommandAction::ForwardStateless => {
            // Reject all non-auth commands before authentication
            use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
            client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
            Ok(AuthHandlerResult::Rejected {
                bytes_written: AUTH_REQUIRED_FOR_COMMAND.len() as u64,
            })
        }
        CommandAction::InterceptAuth(auth_action) => {
            let result = handle_auth_command(
                auth_handler,
                auth_action,
                client_write,
                auth_username,
                auth_state,
            )
            .await?;

            match result {
                AuthResult::Authenticated(bytes) => {
                    on_authentication_success(
                        client_addr,
                        auth_username.clone(),
                        routing_mode,
                        metrics,
                        connection_stats,
                        set_username_fn,
                    );

                    Ok(AuthHandlerResult::Authenticated {
                        bytes_written: bytes.as_u64(),
                        skip_further_checks: true,
                    })
                }
                AuthResult::NotAuthenticated(bytes) => Ok(AuthHandlerResult::NotAuthenticated {
                    bytes_written: bytes.as_u64(),
                }),
            }
        }
        CommandAction::Reject(response) => {
            // Send rejection response inline
            client_write.write_all(response.as_bytes()).await?;
            Ok(AuthHandlerResult::Rejected {
                bytes_written: response.len() as u64,
            })
        }
    }
}

/// Update byte counters and check if metrics should be flushed
///
/// Returns true if metrics should be flushed this iteration
#[inline]
pub fn should_flush_metrics(iteration_count: &mut u32) -> bool {
    *iteration_count += 1;
    *iteration_count >= crate::constants::session::METRICS_FLUSH_INTERVAL
}

/// Session loop state for tracking bytes, auth, and metrics
pub struct SessionLoopState {
    pub client_to_backend: crate::types::ClientToBackendBytes,
    pub backend_to_client: crate::types::BackendToClientBytes,
    pub last_reported_c2b: crate::types::ClientToBackendBytes,
    pub last_reported_b2c: crate::types::BackendToClientBytes,
    pub iteration_count: u32,
    pub auth_username: Option<String>,
    pub skip_auth_check: bool,
}

impl SessionLoopState {
    /// Create new session loop state
    pub fn new(auth_enabled: bool) -> Self {
        use crate::types::{BackendToClientBytes, ClientToBackendBytes};
        Self {
            client_to_backend: ClientToBackendBytes::zero(),
            backend_to_client: BackendToClientBytes::zero(),
            last_reported_c2b: ClientToBackendBytes::zero(),
            last_reported_b2c: BackendToClientBytes::zero(),
            iteration_count: 0,
            auth_username: None,
            skip_auth_check: !auth_enabled,
        }
    }

    /// Create session loop state with initial byte counts (for hybrid mode)
    pub fn from_initial_bytes(
        client_to_backend: u64,
        backend_to_client: u64,
        auth_enabled: bool,
    ) -> Self {
        use crate::types::{BackendToClientBytes, ClientToBackendBytes};
        let c2b = ClientToBackendBytes::new(client_to_backend);
        let b2c = BackendToClientBytes::new(backend_to_client);

        Self {
            client_to_backend: c2b,
            backend_to_client: b2c,
            last_reported_c2b: c2b,
            last_reported_b2c: b2c,
            iteration_count: 0,
            auth_username: None,
            skip_auth_check: !auth_enabled,
        }
    }

    /// Check if metrics should be flushed and reset counter if so
    #[inline]
    pub fn check_and_maybe_flush_metrics(&mut self) -> bool {
        if should_flush_metrics(&mut self.iteration_count) {
            self.iteration_count = 0;
            true
        } else {
            false
        }
    }
}
