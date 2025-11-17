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
    if let AuthAction::RequestPassword(ref username) = auth_action {
        *auth_username = Some(username.clone());
    }

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

        let mut bytes = BytesTransferred::zero();
        bytes.add(CONNECTION_CLOSING.len());
        Ok(QuitStatus::Quit(bytes))
    } else {
        Ok(QuitStatus::Continue)
    }
}

/// Record user command metrics if metrics are enabled
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
/// Close user connection in metrics if enabled
#[inline]
pub(crate) fn close_user_connection(
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) {
    if let Some(m) = metrics {
        m.user_connection_closed(username);
    }
}

/// Log session summary for debugging
#[inline]
pub(crate) fn log_session_summary(
    client_addr: std::net::SocketAddr,
    client_to_backend: u64,
    backend_to_client: u64,
) {
    use tracing::debug;

    if (client_to_backend + backend_to_client) < SMALL_TRANSFER_THRESHOLD {
        debug!(
            "Session summary {} | ↑{} ↓{} | Short session (likely test connection)",
            client_addr,
            crate::formatting::format_bytes(client_to_backend),
            crate::formatting::format_bytes(backend_to_client)
        );
    }
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

// ============================================================================
// Result Pipeline Helpers - Functional error handling
// ============================================================================

/// Check if error is a backend connection error that should remove connection from pool
#[inline]
pub(crate) fn is_backend_error(error: &anyhow::Error) -> bool {
    crate::pool::is_connection_error(error)
}

/// Record backend error metrics for a failed connection
#[inline]
pub(crate) fn record_backend_error(
    backend_id: crate::types::BackendId,
    metrics: &Option<crate::metrics::MetricsCollector>,
    username: Option<&str>,
) {
    if let Some(m) = metrics {
        m.record_error(backend_id);
        record_user_error(metrics, username);
    }
}
