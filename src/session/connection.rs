//! Connection utilities for stateful session handling
//!
//! Handles connection error logging for session handlers.

use tracing::{debug, warn};

use crate::types::TransferMetrics;

/// Log client disconnect/error with appropriate log level and context
pub fn log_client_error(
    client_addr: impl std::fmt::Display,
    username: Option<&str>,
    error: &std::io::Error,
    metrics: TransferMetrics,
) {
    let (c2b, b2c) = metrics.as_tuple();
    let user_info = username.unwrap_or(crate::constants::user::ANONYMOUS);
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            debug!(
                "Client {} ({}) closed connection (EOF) | ↑{} ↓{}",
                client_addr,
                user_info,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::BrokenPipe => {
            debug!(
                "Client {} ({}) connection broken pipe | ↑{} ↓{}",
                client_addr,
                user_info,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            warn!(
                "Client {} ({}) connection reset | ↑{} ↓{}",
                client_addr,
                user_info,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        _ => {
            warn!(
                "Error reading from client {} ({}): {} ({:?}) | ↑{} ↓{}",
                client_addr,
                user_info,
                error,
                error.kind(),
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BackendToClientBytes, ClientToBackendBytes};
    #[test]
    fn test_log_client_error_no_panic() {
        let metrics = TransferMetrics {
            client_to_backend: ClientToBackendBytes::new(100),
            backend_to_client: BackendToClientBytes::new(200),
        };

        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            let err = std::io::Error::new(kind, "test error");
            log_client_error("127.0.0.1:1234", Some("testuser"), &err, metrics);
            log_client_error("127.0.0.1:1234", None, &err, metrics);
        }
    }
}
