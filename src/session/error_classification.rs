//! Error classification utilities for routing errors
//!
//! Provides helpers to classify errors wrapped in anyhow::Error

use crate::connection_error::ConnectionError;
use std::io::ErrorKind;

/// Classify an anyhow error and determine appropriate handling
pub struct ErrorClassifier;

impl ErrorClassifier {
    /// Check if error is a client disconnect (broken pipe or connection reset)
    ///
    /// When a client disconnects, we can receive either:
    /// - `BrokenPipe` (os error 32) - writing to a closed socket
    /// - `ConnectionReset` (os error 104) - peer forcibly closed the connection
    pub fn is_client_disconnect(error: &anyhow::Error) -> bool {
        // First check if it's a ConnectionError
        if let Some(conn_err) = error.downcast_ref::<ConnectionError>() {
            return conn_err.is_client_disconnect();
        }

        // Check raw IO error for both BrokenPipe and ConnectionReset
        if let Some(io_err) = error.downcast_ref::<std::io::Error>() {
            return matches!(
                io_err.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            );
        }

        false
    }

    /// Check if error is an authentication failure
    pub fn is_authentication_error(error: &anyhow::Error) -> bool {
        // Check for ConnectionError::AuthenticationFailed
        if let Some(conn_err) = error.downcast_ref::<ConnectionError>() {
            return conn_err.is_authentication_error();
        }

        // Check error message as fallback
        let error_str = error.to_string();
        error_str.contains("Auth failed") || error_str.contains("Authentication Failed")
    }

    /// Check if error is a network/connectivity issue
    pub fn is_network_error(error: &anyhow::Error) -> bool {
        if let Some(conn_err) = error.downcast_ref::<ConnectionError>() {
            return conn_err.is_network_error();
        }
        false
    }

    /// Check if we should skip sending error response to client
    pub fn should_skip_client_error_response(error: &anyhow::Error) -> bool {
        Self::is_client_disconnect(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_client_disconnect_with_io_error() {
        let io_err = std::io::Error::new(ErrorKind::BrokenPipe, "broken pipe");
        let err: anyhow::Error = io_err.into();
        assert!(ErrorClassifier::is_client_disconnect(&err));
    }

    #[test]
    fn test_is_client_disconnect_with_connection_error() {
        let io_err = std::io::Error::new(ErrorKind::BrokenPipe, "broken pipe");
        let conn_err = ConnectionError::IoError(io_err);
        let err: anyhow::Error = conn_err.into();
        assert!(ErrorClassifier::is_client_disconnect(&err));
    }

    #[test]
    fn test_is_authentication_error_with_connection_error() {
        let conn_err = ConnectionError::AuthenticationFailed {
            backend: "test".to_string(),
            response: "502 failed".to_string(),
        };
        let err: anyhow::Error = conn_err.into();
        assert!(ErrorClassifier::is_authentication_error(&err));
    }

    #[test]
    fn test_is_authentication_error_with_message() {
        let err = anyhow::anyhow!("Auth failed: invalid credentials");
        assert!(ErrorClassifier::is_authentication_error(&err));
    }

    #[test]
    fn test_should_skip_client_error_response() {
        let io_err = std::io::Error::new(ErrorKind::BrokenPipe, "broken");
        let err: anyhow::Error = io_err.into();
        assert!(ErrorClassifier::should_skip_client_error_response(&err));

        let other_err = anyhow::anyhow!("some other error");
        assert!(!ErrorClassifier::should_skip_client_error_response(
            &other_err
        ));
    }

    #[test]
    fn test_client_disconnect_classification() {
        // Verify broken pipe is detected as client disconnect
        let broken_pipe = std::io::Error::new(ErrorKind::BrokenPipe, "pipe");
        let err: anyhow::Error = broken_pipe.into();
        assert!(ErrorClassifier::is_client_disconnect(&err));
        assert!(ErrorClassifier::should_skip_client_error_response(&err));

        // ConnectionReset is ALSO a client disconnect (peer forcibly closed)
        let reset = std::io::Error::new(ErrorKind::ConnectionReset, "reset");
        let err: anyhow::Error = reset.into();
        assert!(ErrorClassifier::is_client_disconnect(&err));
        assert!(ErrorClassifier::should_skip_client_error_response(&err));
    }

    #[test]
    fn test_error_classification_layering() {
        // Test that we can distinguish between different error types
        // for proper logging at different levels

        // 1. Client disconnect (broken pipe) - should be DEBUG in streaming layer
        let broken_pipe = std::io::Error::new(ErrorKind::BrokenPipe, "pipe");
        let err: anyhow::Error = broken_pipe.into();
        assert!(ErrorClassifier::is_client_disconnect(&err));
        assert!(!ErrorClassifier::is_authentication_error(&err));
        assert!(!ErrorClassifier::is_network_error(&err));

        // 2. Auth failure - should be ERROR
        let auth_fail = ConnectionError::AuthenticationFailed {
            backend: "test".to_string(),
            response: "nope".to_string(),
        };
        let err: anyhow::Error = auth_fail.into();
        assert!(!ErrorClassifier::is_client_disconnect(&err));
        assert!(ErrorClassifier::is_authentication_error(&err));
        assert!(!ErrorClassifier::is_network_error(&err));

        // 3. Network error - should be WARN
        let net_err = ConnectionError::TcpConnect {
            host: "test".to_string(),
            port: 119,
            source: std::io::Error::new(ErrorKind::ConnectionRefused, "refused"),
        };
        let err: anyhow::Error = net_err.into();
        assert!(!ErrorClassifier::is_client_disconnect(&err));
        assert!(!ErrorClassifier::is_authentication_error(&err));
        assert!(ErrorClassifier::is_network_error(&err));
    }
}
