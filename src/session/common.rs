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

    // =========================================================================
    // AuthHandlerResult tests
    // =========================================================================

    #[test]
    fn test_auth_handler_result_bytes_written() {
        let auth = AuthHandlerResult::Authenticated {
            bytes_written: 100,
            skip_further_checks: true,
        };
        assert_eq!(auth.bytes_written(), 100);

        let not_auth = AuthHandlerResult::NotAuthenticated { bytes_written: 50 };
        assert_eq!(not_auth.bytes_written(), 50);

        let rejected = AuthHandlerResult::Rejected { bytes_written: 25 };
        assert_eq!(rejected.bytes_written(), 25);
    }

    #[test]
    fn test_auth_handler_result_should_skip_further_checks() {
        let skip = AuthHandlerResult::Authenticated {
            bytes_written: 100,
            skip_further_checks: true,
        };
        assert!(skip.should_skip_further_checks());

        let no_skip = AuthHandlerResult::Authenticated {
            bytes_written: 100,
            skip_further_checks: false,
        };
        assert!(!no_skip.should_skip_further_checks());

        let not_auth = AuthHandlerResult::NotAuthenticated { bytes_written: 50 };
        assert!(!not_auth.should_skip_further_checks());

        let rejected = AuthHandlerResult::Rejected { bytes_written: 25 };
        assert!(!rejected.should_skip_further_checks());
    }

    // =========================================================================
    // SessionLoopState tests
    // =========================================================================

    #[test]
    fn test_session_loop_state_new() {
        let state = SessionLoopState::new(true);
        assert_eq!(state.client_to_backend.as_u64(), 0);
        assert_eq!(state.backend_to_client.as_u64(), 0);
        assert!(!state.skip_auth_check); // Auth enabled = don't skip
        assert!(state.auth_username.is_none());

        let state2 = SessionLoopState::new(false);
        assert!(state2.skip_auth_check); // Auth disabled = skip
    }

    #[test]
    fn test_session_loop_state_default() {
        let state = SessionLoopState::default();
        assert_eq!(state.client_to_backend.as_u64(), 0);
        assert!(state.skip_auth_check); // Default = auth disabled
    }

    #[test]
    fn test_session_loop_state_builder_pattern() {
        let state = SessionLoopState::new(false).with_initial_bytes(1000, 500);

        assert_eq!(state.client_to_backend.as_u64(), 1000);
        assert_eq!(state.backend_to_client.as_u64(), 500);
    }

    #[test]
    fn test_session_loop_state_from_initial_bytes() {
        let state = SessionLoopState::from_initial_bytes(100, 200, true);
        assert_eq!(state.client_to_backend.as_u64(), 100);
        assert_eq!(state.backend_to_client.as_u64(), 200);
        assert_eq!(state.last_reported_c2b.as_u64(), 100);
        assert_eq!(state.last_reported_b2c.as_u64(), 200);
        assert!(!state.skip_auth_check);
    }

    #[test]
    fn test_session_loop_state_add_bytes() {
        let mut state = SessionLoopState::new(false);

        state.add_client_to_backend(100);
        assert_eq!(state.client_to_backend.as_u64(), 100);

        state.add_backend_to_client(200);
        assert_eq!(state.backend_to_client.as_u64(), 200);

        // Cumulative
        state.add_client_to_backend(50);
        state.add_backend_to_client(50);
        assert_eq!(state.client_to_backend.as_u64(), 150);
        assert_eq!(state.backend_to_client.as_u64(), 250);
    }

    #[test]
    fn test_session_loop_state_mark_authenticated() {
        let mut state = SessionLoopState::new(true);
        assert!(!state.skip_auth_check);

        state.mark_authenticated();
        assert!(state.skip_auth_check);
    }

    #[test]
    fn test_session_loop_state_apply_auth_result() {
        let mut state = SessionLoopState::new(true);
        assert!(!state.skip_auth_check);
        assert_eq!(state.backend_to_client.as_u64(), 0);

        // Authenticated result should update bytes and skip flag
        let result = AuthHandlerResult::Authenticated {
            bytes_written: 100,
            skip_further_checks: true,
        };
        let bytes = state.apply_auth_result(&result);

        assert_eq!(bytes, 100);
        assert_eq!(state.backend_to_client.as_u64(), 100);
        assert!(state.skip_auth_check);
    }

    #[test]
    fn test_session_loop_state_apply_auth_result_not_authenticated() {
        let mut state = SessionLoopState::new(true);

        let result = AuthHandlerResult::NotAuthenticated { bytes_written: 50 };
        state.apply_auth_result(&result);

        assert_eq!(state.backend_to_client.as_u64(), 50);
        assert!(!state.skip_auth_check); // Still need to check
    }

    #[test]
    fn test_session_loop_state_into_metrics() {
        let state = SessionLoopState::new(false).with_initial_bytes(1000, 2000);

        let metrics = state.into_metrics();
        assert_eq!(metrics.client_to_backend.as_u64(), 1000);
        assert_eq!(metrics.backend_to_client.as_u64(), 2000);
    }

    #[test]
    fn test_session_loop_state_metrics_flush_interval() {
        use crate::constants::session::METRICS_FLUSH_INTERVAL;

        let mut state = SessionLoopState::new(false);

        // Should return false until we hit the interval
        for _ in 0..(METRICS_FLUSH_INTERVAL - 1) {
            assert!(!state.check_and_maybe_flush_metrics());
        }

        // Should return true at the interval
        assert!(state.check_and_maybe_flush_metrics());

        // Counter should reset, so next METRICS_FLUSH_INTERVAL-1 should be false
        assert!(!state.check_and_maybe_flush_metrics());
    }
}

// ============================================================================
// Session Loop Helpers
// ============================================================================

/// Result of handling an authentication command in a session loop
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl AuthHandlerResult {
    /// Get the number of bytes written regardless of result type
    #[inline]
    pub const fn bytes_written(&self) -> u64 {
        match self {
            Self::Authenticated { bytes_written, .. }
            | Self::NotAuthenticated { bytes_written }
            | Self::Rejected { bytes_written } => *bytes_written,
        }
    }

    /// Check if should skip further auth checks
    #[inline]
    pub const fn should_skip_further_checks(&self) -> bool {
        matches!(
            self,
            Self::Authenticated {
                skip_further_checks: true,
                ..
            }
        )
    }
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

/// Session loop state for tracking bytes, auth, and metrics
///
/// This struct encapsulates all mutable state needed during a session loop,
/// making it easy to pass around and test in isolation.
///
/// # Example
/// ```ignore
/// let state = SessionLoopState::new(auth_enabled)
///     .with_initial_bytes(1000, 500);
/// ```
#[derive(Debug, Clone)]
pub struct SessionLoopState {
    /// Bytes sent from client to backend
    pub client_to_backend: crate::types::ClientToBackendBytes,
    /// Bytes sent from backend to client
    pub backend_to_client: crate::types::BackendToClientBytes,
    /// Last reported client-to-backend bytes (for incremental metrics)
    pub last_reported_c2b: crate::types::ClientToBackendBytes,
    /// Last reported backend-to-client bytes (for incremental metrics)
    pub last_reported_b2c: crate::types::BackendToClientBytes,
    /// Iteration counter for metrics flush timing
    iteration_count: u32,
    /// Username from AUTHINFO USER command (if any)
    pub auth_username: Option<String>,
    /// Whether to skip auth checking (optimization after first auth)
    pub skip_auth_check: bool,
}

impl Default for SessionLoopState {
    fn default() -> Self {
        Self::new(false)
    }
}

impl SessionLoopState {
    /// Create new session loop state
    ///
    /// # Arguments
    /// * `auth_enabled` - If true, auth checking starts enabled; if false, it's skipped
    #[must_use]
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

    /// Create session loop state with initial byte counts
    ///
    /// Used by hybrid mode when switching from per-command to stateful,
    /// to carry forward the bytes already transferred.
    #[must_use]
    pub fn from_initial_bytes(
        client_to_backend: u64,
        backend_to_client: u64,
        auth_enabled: bool,
    ) -> Self {
        Self::new(auth_enabled).with_initial_bytes(client_to_backend, backend_to_client)
    }

    /// Builder method: set initial byte counts
    #[must_use]
    pub fn with_initial_bytes(mut self, c2b: u64, b2c: u64) -> Self {
        use crate::types::{BackendToClientBytes, ClientToBackendBytes};
        self.client_to_backend = ClientToBackendBytes::new(c2b);
        self.backend_to_client = BackendToClientBytes::new(b2c);
        self.last_reported_c2b = self.client_to_backend;
        self.last_reported_b2c = self.backend_to_client;
        self
    }

    /// Check if metrics should be flushed and reset counter if so
    ///
    /// Returns `true` every `METRICS_FLUSH_INTERVAL` iterations.
    #[inline]
    pub fn check_and_maybe_flush_metrics(&mut self) -> bool {
        self.iteration_count += 1;
        if self.iteration_count >= crate::constants::session::METRICS_FLUSH_INTERVAL {
            self.iteration_count = 0;
            true
        } else {
            false
        }
    }

    /// Add bytes to client-to-backend counter
    #[inline]
    pub fn add_client_to_backend(&mut self, bytes: usize) {
        self.client_to_backend = self.client_to_backend.add(bytes);
    }

    /// Add bytes to backend-to-client counter
    #[inline]
    pub fn add_backend_to_client(&mut self, bytes: u64) {
        self.backend_to_client = self.backend_to_client.add_u64(bytes);
    }

    /// Convert to final transfer metrics
    #[must_use]
    pub fn into_metrics(self) -> crate::types::TransferMetrics {
        crate::types::TransferMetrics {
            client_to_backend: self.client_to_backend,
            backend_to_client: self.backend_to_client,
        }
    }

    /// Mark authentication as complete (skip future checks)
    #[inline]
    pub fn mark_authenticated(&mut self) {
        self.skip_auth_check = true;
    }

    /// Update state based on auth handler result
    ///
    /// Returns the bytes written for convenience in chaining.
    pub fn apply_auth_result(&mut self, result: &AuthHandlerResult) -> u64 {
        let bytes = result.bytes_written();
        self.add_backend_to_client(bytes);
        if result.should_skip_further_checks() {
            self.mark_authenticated();
        }
        bytes
    }
}
