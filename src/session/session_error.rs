//! Session-level error enum with explicit client-disconnect classification.
//!
//! Replaces the `is_client_disconnect_error` downcast pattern with a typed enum
//! that carries the disconnect signal through the entire handler stack without
//! any `downcast_ref` inspection at each call site.

use std::fmt;

/// Session-level error that carries the client-disconnect signal through the handler stack.
///
/// Only two cases matter for control flow:
/// - `ClientDisconnect` — client closed connection; don't log, don't retry, propagate cleanly
/// - `Backend` — log, maybe retry, handle as backend/protocol error
#[derive(Debug)]
pub enum SessionError {
    /// Client disconnected (`BrokenPipe`, `ConnectionReset`).
    ///
    /// Not a real error — normal operation. Should not be logged as a warning.
    ClientDisconnect(std::io::Error),

    /// Any other error (backend I/O, protocol, timeout, pool exhaustion, etc.)
    Backend(anyhow::Error),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientDisconnect(e) => write!(f, "client disconnected: {e}"),
            Self::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientDisconnect(e) => Some(e),
            Self::Backend(e) => e.source(),
        }
    }
}

/// Convert `anyhow::Error` into `SessionError`, classifying client disconnects.
///
/// Tries to downcast to `io::Error` (most common) and then to `ConnectionError`.
/// Disconnect kinds (`BrokenPipe`, `ConnectionReset`) become `ClientDisconnect`.
/// Everything else becomes `Backend`.
impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        use crate::connection_error::{ConnectionError, is_disconnect_kind};

        // Fast path: bare io::Error (most common for client disconnects in stateful path)
        match e.downcast::<std::io::Error>() {
            Ok(io_err) if is_disconnect_kind(io_err.kind()) => Self::ClientDisconnect(io_err),
            Ok(io_err) => Self::Backend(io_err.into()),
            Err(e) => {
                // Try ConnectionError::IoError
                match e.downcast::<ConnectionError>() {
                    Ok(ConnectionError::IoError(io_err)) if is_disconnect_kind(io_err.kind()) => {
                        Self::ClientDisconnect(io_err)
                    }
                    Ok(conn_err) => Self::Backend(conn_err.into()),
                    Err(e) => Self::Backend(e),
                }
            }
        }
    }
}

/// Convert `StreamingError` to `SessionError`, preserving the disconnect signal.
impl From<crate::session::streaming::StreamingError> for SessionError {
    fn from(e: crate::session::streaming::StreamingError) -> Self {
        match e {
            crate::session::streaming::StreamingError::ClientDisconnect(io_err) => {
                Self::ClientDisconnect(io_err)
            }
            other => Self::Backend(other.into_anyhow()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn client_disconnect_from_broken_pipe_io_error() {
        let io_err = std::io::Error::from(ErrorKind::BrokenPipe);
        let e = SessionError::from(anyhow::Error::from(io_err));
        assert!(matches!(e, SessionError::ClientDisconnect(_)));
    }

    #[test]
    fn client_disconnect_from_connection_reset_io_error() {
        let io_err = std::io::Error::from(ErrorKind::ConnectionReset);
        let e = SessionError::from(anyhow::Error::from(io_err));
        assert!(matches!(e, SessionError::ClientDisconnect(_)));
    }

    #[test]
    fn backend_from_non_disconnect_io_error() {
        let io_err = std::io::Error::from(ErrorKind::TimedOut);
        let e = SessionError::from(anyhow::Error::from(io_err));
        assert!(matches!(e, SessionError::Backend(_)));
    }

    #[test]
    fn backend_from_non_io_error() {
        let e = SessionError::from(anyhow::anyhow!("generic error"));
        assert!(matches!(e, SessionError::Backend(_)));
    }

    #[test]
    fn client_disconnect_from_streaming_error() {
        let io_err = std::io::Error::from(ErrorKind::BrokenPipe);
        let streaming_err = crate::session::streaming::StreamingError::ClientDisconnect(io_err);
        let e = SessionError::from(streaming_err);
        assert!(matches!(e, SessionError::ClientDisconnect(_)));
    }

    #[test]
    fn backend_from_streaming_backend_eof() {
        let streaming_err = crate::session::streaming::StreamingError::BackendEof {
            backend_id: crate::types::BackendId::from_index(0),
            bytes_received: 100,
        };
        let e = SessionError::from(streaming_err);
        assert!(matches!(e, SessionError::Backend(_)));
    }
}
