//! Common utilities shared across handler modules

use crate::auth::AuthHandler;
use crate::command::AuthAction;
use crate::protocol::{RequestContext, RequestKind, RequestResponseMetadata, StatusCode, codes};
use crate::types::BackendToClientBytes;

use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Result of handling an auth command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthResult {
    /// Authentication succeeded
    Authenticated {
        bytes: BackendToClientBytes,
        response: RequestResponseMetadata,
    },
    /// Authentication failed or not required yet
    NotAuthenticated {
        bytes: BackendToClientBytes,
        response: RequestResponseMetadata,
    },
}

impl AuthResult {
    #[must_use]
    pub const fn bytes_written(self) -> BackendToClientBytes {
        match self {
            Self::Authenticated { bytes, .. } | Self::NotAuthenticated { bytes, .. } => bytes,
        }
    }

    #[must_use]
    pub(crate) const fn response_metadata(self) -> RequestResponseMetadata {
        match self {
            Self::Authenticated { response, .. } | Self::NotAuthenticated { response, .. } => {
                response
            }
        }
    }
}

/// Result of checking for QUIT command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitStatus {
    /// QUIT command was detected and response sent (contains bytes written)
    Quit(BackendToClientBytes),
    /// Not a QUIT command
    Continue,
}

/// Handle AUTHINFO command and update auth state
pub async fn handle_auth_command<W>(
    auth_handler: &Arc<AuthHandler>,
    auth_action: AuthAction<'_>,
    client_write: &mut W,
    auth_username: &mut Option<String>,
    auth_state: &crate::session::AuthState,
) -> Result<AuthResult>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // RFC 4643 §2.2: Once a client has successfully authenticated, any subsequent
    // AUTHINFO command must be rejected with 502.
    if auth_state.is_authenticated() {
        use crate::protocol::AUTH_ALREADY_AUTHENTICATED;
        client_write.write_all(AUTH_ALREADY_AUTHENTICATED).await?;
        let bytes = BackendToClientBytes::new(AUTH_ALREADY_AUTHENTICATED.len() as u64);
        return Ok(AuthResult::NotAuthenticated {
            bytes,
            response: local_response_metadata(codes::ACCESS_DENIED, AUTH_ALREADY_AUTHENTICATED),
        });
    }

    let had_username = auth_username.is_some();
    if let AuthAction::RequestPassword(username) = auth_action {
        *auth_username = Some(username.to_string());
    }

    let (bytes, auth_success) = auth_handler
        .handle_auth_command(auth_action, client_write, auth_username.as_deref())
        .await?;

    if auth_success && let Some(ref username) = *auth_username {
        auth_state.mark_authenticated(username.clone());
    }

    let bytes_written = BackendToClientBytes::new(bytes as u64);
    let response = auth_response_metadata(auth_action, had_username, auth_success);

    Ok(if auth_success {
        AuthResult::Authenticated {
            bytes: bytes_written,
            response,
        }
    } else {
        AuthResult::NotAuthenticated {
            bytes: bytes_written,
            response,
        }
    })
}

fn local_response_metadata(status: u16, response: &[u8]) -> RequestResponseMetadata {
    RequestResponseMetadata::new(StatusCode::new(status), response.len().into())
}

fn auth_response_metadata(
    auth_action: AuthAction<'_>,
    had_username: bool,
    auth_success: bool,
) -> RequestResponseMetadata {
    match auth_action {
        AuthAction::RequestPassword(_) => {
            local_response_metadata(codes::PASSWORD_REQUIRED, crate::protocol::AUTH_REQUIRED)
        }
        AuthAction::ValidateAndRespond { .. } if !had_username => local_response_metadata(
            codes::AUTH_OUT_OF_SEQUENCE,
            crate::protocol::AUTH_OUT_OF_SEQUENCE,
        ),
        AuthAction::ValidateAndRespond { .. } if auth_success => {
            local_response_metadata(codes::AUTH_ACCEPTED, crate::protocol::AUTH_ACCEPTED)
        }
        AuthAction::ValidateAndRespond { .. } => {
            local_response_metadata(codes::AUTH_REJECTED, crate::protocol::AUTH_FAILED)
        }
        AuthAction::UnknownSubcommand => local_response_metadata(
            codes::COMMAND_SYNTAX_ERROR,
            crate::protocol::AUTH_UNKNOWN_SUBCOMMAND,
        ),
    }
}

/// Check if command is QUIT and send closing response
pub async fn handle_quit_command<W>(
    request: &RequestContext,
    client_write: &mut W,
) -> Result<QuitStatus>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if request.kind() == RequestKind::Quit {
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
#[allow(clippy::needless_pass_by_value)]
pub fn on_authentication_success(
    client_addr: impl std::fmt::Display,
    username: Option<String>,
    routing_mode: crate::config::RoutingMode,
    metrics: &crate::metrics::MetricsCollector,
    connection_stats: Option<&crate::metrics::ConnectionStatsAggregator>,
    set_username_fn: impl FnOnce(Option<String>),
) {
    use tracing::debug;

    // The auth path already owns the username here. Keeping ownership lets us
    // hand the same value to session state while also reusing it for metrics and
    // logging without threading extra lifetimes through the handler chain.
    debug!("Client {} authenticated as: {:?}", client_addr, username);

    // Store username in session
    set_username_fn(username.clone());

    // Record connection for aggregation (after auth so we have username)
    if let Some(stats) = connection_stats {
        stats.record_connection(username.as_deref(), routing_mode.short_name());
    }

    // Track user connection in metrics
    metrics.user_connection_opened(username.as_deref());
    debug!(
        "Client {} opened connection for user: {:?}",
        client_addr, username
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_result_equality() {
        let response = crate::protocol::RequestResponseMetadata::new(
            crate::protocol::StatusCode::new(281),
            27_usize.into(),
        );
        let auth1 = AuthResult::Authenticated {
            bytes: BackendToClientBytes::new(100),
            response,
        };
        let auth2 = AuthResult::Authenticated {
            bytes: BackendToClientBytes::new(100),
            response,
        };
        let not_auth = AuthResult::NotAuthenticated {
            bytes: BackendToClientBytes::new(100),
            response,
        };

        assert_eq!(auth1, auth2);
        assert_ne!(auth1, not_auth);
        assert_eq!(auth1.bytes_written(), BackendToClientBytes::new(100));
        assert_eq!(auth1.response_metadata(), response);
    }

    #[test]
    fn test_quit_status_equality() {
        let quit1 = QuitStatus::Quit(BackendToClientBytes::new(50));
        let quit2 = QuitStatus::Quit(BackendToClientBytes::new(50));
        let cont = QuitStatus::Continue;

        assert_eq!(quit1, quit2);
        assert_ne!(quit1, cont);
    }

    #[tokio::test]
    async fn handle_quit_command_uses_typed_request_kind() {
        let request = RequestContext::parse(b"quit\r\n").expect("valid request line");
        let mut written = Vec::new();

        let status = handle_quit_command(&request, &mut written).await.unwrap();

        assert_eq!(
            status,
            QuitStatus::Quit(BackendToClientBytes::new(
                crate::protocol::CONNECTION_CLOSING.len() as u64
            ))
        );
        assert_eq!(written, crate::protocol::CONNECTION_CLOSING);
    }

    #[tokio::test]
    async fn handle_quit_command_ignores_non_quit_request_contexts() {
        let request = RequestContext::parse(b"HELP\r\n").expect("valid request line");
        let mut written = Vec::new();

        let status = handle_quit_command(&request, &mut written).await.unwrap();

        assert_eq!(status, QuitStatus::Continue);
        assert!(written.is_empty());
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

/// Context for stateful authentication checks
///
/// Groups the session-level references needed by [`handle_stateful_auth_check`]
/// to keep its parameter list concise.
pub struct AuthCheckContext<'a> {
    pub auth_handler: &'a std::sync::Arc<crate::auth::AuthHandler>,
    pub auth_state: &'a crate::session::AuthState,
    pub routing_mode: &'a crate::config::RoutingMode,
    pub metrics: &'a crate::metrics::MetricsCollector,
    pub connection_stats: Option<&'a crate::metrics::ConnectionStatsAggregator>,
}

/// Handle authentication logic for a command in a stateful session
///
/// This encapsulates the common pattern of:
/// 1. Checking if authenticated
/// 2. Handling auth commands
/// 3. Rejecting non-auth commands when not authenticated
pub async fn handle_stateful_auth_check<W>(
    request: &crate::protocol::RequestContext,
    client_write: &mut W,
    auth_username: &mut Option<String>,
    ctx: &AuthCheckContext<'_>,
    client_addr: impl std::fmt::Display + Clone,
    set_username_fn: impl FnOnce(Option<String>),
) -> anyhow::Result<AuthHandlerResult>
where
    W: AsyncWriteExt + Unpin,
{
    use crate::command::{CommandAction, CommandHandler};

    let action = CommandHandler::classify_request(request);
    match action {
        CommandAction::ForwardStateless => {
            // Reject all non-auth commands before authentication
            use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
            client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
            Ok(AuthHandlerResult::Rejected {
                bytes_written: AUTH_REQUIRED_FOR_COMMAND.len() as u64,
            })
        }
        CommandAction::InterceptCapabilities => {
            // RFC 4643 §3.1: CAPABILITIES must be accessible before authentication.
            // Auth is enabled and client is not yet authenticated → include AUTHINFO.
            use crate::protocol::CAPABILITIES_WITH_AUTHINFO;
            client_write.write_all(CAPABILITIES_WITH_AUTHINFO).await?;
            Ok(AuthHandlerResult::Rejected {
                bytes_written: CAPABILITIES_WITH_AUTHINFO.len() as u64,
            })
        }
        CommandAction::InterceptAuth(auth_action) => {
            let result = handle_auth_command(
                ctx.auth_handler,
                auth_action,
                client_write,
                auth_username,
                ctx.auth_state,
            )
            .await?;

            match result {
                AuthResult::Authenticated { bytes, .. } => {
                    on_authentication_success(
                        client_addr,
                        auth_username.clone(),
                        *ctx.routing_mode,
                        ctx.metrics,
                        ctx.connection_stats,
                        set_username_fn,
                    );

                    Ok(AuthHandlerResult::Authenticated {
                        bytes_written: bytes.as_u64(),
                        skip_further_checks: true,
                    })
                }
                AuthResult::NotAuthenticated { bytes, .. } => {
                    Ok(AuthHandlerResult::NotAuthenticated {
                        bytes_written: bytes.as_u64(),
                    })
                }
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
