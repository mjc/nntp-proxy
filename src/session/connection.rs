//! Connection utilities for stateful session handling
//!
//! Handles connection error logging for session handlers.

use tracing::debug;

use crate::types::TransferMetrics;

/// Log client disconnect/error with context.
///
/// Every outcome is at debug: a client resetting or failing its own socket is
/// normal on the internet and is nothing an operator can act on.
///
/// Usernames are deliberately not logged.
pub fn log_client_error(
    client_addr: impl std::fmt::Display,
    error: &std::io::Error,
    metrics: TransferMetrics,
) {
    let (c2b, b2c) = metrics.as_tuple();
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            debug!(
                "Client {} closed connection (EOF) | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::BrokenPipe => {
            debug!(
                "Client {} connection broken pipe | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        std::io::ErrorKind::ConnectionReset => {
            debug!(
                "Client {} connection reset | ↑{} ↓{}",
                client_addr,
                crate::formatting::format_bytes(c2b),
                crate::formatting::format_bytes(b2c)
            );
        }
        _ => {
            debug!(
                "Error reading from client {}: {} ({:?}) | ↑{} ↓{}",
                client_addr,
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
            log_client_error("127.0.0.1:1234", &err, metrics);
        }
    }
}
