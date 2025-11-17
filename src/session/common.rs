//! Common utilities shared across handler modules

use crate::auth::AuthHandler;
use crate::command::AuthAction;
use crate::types::BytesTransferred;
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Threshold for logging detailed transfer info (bytes)
/// Transfers under this size are considered "small" (test connections, etc.)
pub(crate) const SMALL_TRANSFER_THRESHOLD: u64 = 500;

/// Result of handling an auth command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthResult {
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
pub(crate) enum QuitStatus {
    /// QUIT command was detected and response sent (contains bytes written)
    Quit(BytesTransferred),
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
pub(crate) async fn handle_auth_command<W>(
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

        let mut bytes = BytesTransferred::zero();
        bytes.add(CONNECTION_CLOSING.len());
        Ok(QuitStatus::Quit(bytes))
    } else {
        Ok(QuitStatus::Continue)
    }
}

/// Record user command metrics if metrics are enabled
///
/// Functional helper that encapsulates the option-checking pattern
#[inline]
pub(crate) fn record_user_command(
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) {
    if let Some(m) = metrics {
        m.user_command(username);
    }
}

/// Record user error metrics if metrics are enabled
///
/// Functional helper that encapsulates the option-checking pattern
#[inline]
pub(crate) fn record_user_error(
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) {
    if let Some(m) = metrics {
        m.user_error(username);
    }
}

/// Record user byte transfer metrics if metrics are enabled
///
/// Functional helper that records both sent and received bytes
#[inline]
pub(crate) fn record_user_bytes(
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
    sent: u64,
    received: u64,
) {
    if let Some(m) = metrics {
        m.user_bytes_sent(username, sent);
        m.user_bytes_received(username, received);
    }
}

/// Handle backend connection error with appropriate logging and metrics
///
/// Returns true if connection should be removed from pool
#[must_use]
pub(crate) fn handle_backend_error(
    error: &anyhow::Error,
    got_backend_data: bool,
    backend_id: crate::types::BackendId,
    client_addr: std::net::SocketAddr,
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) -> bool {
    use crate::pool::is_connection_error;
    use tracing::{debug, warn};

    if !is_connection_error(error) {
        return false;
    }

    match got_backend_data {
        true => {
            debug!(
                "Client {} disconnected while receiving data from backend {:?} - backend connection is healthy",
                client_addr, backend_id
            );
            false // Don't remove - client disconnect, not backend issue
        }
        false => {
            warn!(
                "Backend connection error for client {}, backend {:?}: {} - removing connection from pool",
                client_addr, backend_id, error
            );
            // Record error in metrics
            if let Some(m) = metrics {
                m.record_error(backend_id);
                record_user_error(metrics, username);
            }
            true // Remove from pool - backend issue
        }
    }
}

/// Process command execution result using functional pipeline
///
/// Returns (Result mapped to backend_id, should_remove_from_pool)
#[inline]
pub(crate) fn process_command_result(
    result: &Result<(), anyhow::Error>,
    got_backend_data: bool,
    backend_id: crate::types::BackendId,
    client_addr: std::net::SocketAddr,
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) -> (Result<crate::types::BackendId, anyhow::Error>, bool) {
    let should_remove = handle_backend_error_and_cleanup(
        result,
        got_backend_data,
        backend_id,
        client_addr,
        metrics,
        username,
    );

    let result_mapped = result
        .as_ref()
        .map(|_| backend_id)
        .map_err(|e| anyhow::anyhow!("{}", e));

    (result_mapped, should_remove)
}

/// Handle backend error and remove connection if needed
///
/// Separate function to handle the pool removal properly
#[inline]
pub(crate) fn handle_backend_error_and_cleanup(
    result: &Result<(), anyhow::Error>,
    got_backend_data: bool,
    backend_id: crate::types::BackendId,
    client_addr: std::net::SocketAddr,
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) -> bool {
    result
        .as_ref()
        .err()
        .map(|e| {
            handle_backend_error(
                e,
                got_backend_data,
                backend_id,
                client_addr,
                metrics,
                username,
            )
        })
        .unwrap_or(false)
}

/// Handle successful authentication with all side effects
///
/// Sets username, records connection stats, updates metrics
pub(crate) fn on_authentication_success(
    client_addr: std::net::SocketAddr,
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
