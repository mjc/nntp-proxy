//! Error classification utilities for routing errors
//!
//! Provides helpers to classify errors wrapped in anyhow::Error

use crate::connection_error::ConnectionError;
use std::io::ErrorKind;

/// Classify an anyhow error and determine appropriate handling
pub struct ErrorClassifier;

impl ErrorClassifier {
    /// Check if error is a client disconnect (broken pipe)
    pub fn is_client_disconnect(error: &anyhow::Error) -> bool {
        // First check if it's a ConnectionError
        if let Some(conn_err) = error.downcast_ref::<ConnectionError>() {
            return conn_err.is_client_disconnect();
        }

        // Check raw IO error
        if let Some(io_err) = error.downcast_ref::<std::io::Error>() {
            return io_err.kind() == ErrorKind::BrokenPipe;
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

    /// Get appropriate log level for error
    pub fn log_level(error: &anyhow::Error) -> tracing::Level {
        if let Some(conn_err) = error.downcast_ref::<ConnectionError>() {
            return conn_err.log_level();
        }

        // Check IO errors
        if let Some(io_err) = error.downcast_ref::<std::io::Error>() {
            return match io_err.kind() {
                ErrorKind::BrokenPipe => tracing::Level::DEBUG,
                ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted => tracing::Level::WARN,
                _ => tracing::Level::ERROR,
            };
        }

        // Default to ERROR for unknown errors
        tracing::Level::ERROR
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
    fn test_log_level_client_disconnect() {
        let io_err = std::io::Error::new(ErrorKind::BrokenPipe, "broken");
        let err: anyhow::Error = io_err.into();
        assert_eq!(ErrorClassifier::log_level(&err), tracing::Level::DEBUG);
    }

    #[test]
    fn test_log_level_auth_error() {
        let conn_err = ConnectionError::AuthenticationFailed {
            backend: "test".to_string(),
            response: "failed".to_string(),
        };
        let err: anyhow::Error = conn_err.into();
        assert_eq!(ErrorClassifier::log_level(&err), tracing::Level::ERROR);
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
}
