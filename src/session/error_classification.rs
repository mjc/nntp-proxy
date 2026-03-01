//! Error classification utilities for routing errors
//!
//! Provides helpers to classify errors wrapped in `anyhow::Error`

use crate::connection_error::{ConnectionError, is_disconnect_kind};

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

        // Check raw IO error using centralized disconnect kinds
        if let Some(io_err) = error.downcast_ref::<std::io::Error>() {
            return is_disconnect_kind(io_err.kind());
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
    use std::io::ErrorKind;

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
}
