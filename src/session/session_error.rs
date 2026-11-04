//! Session-level error enum with explicit client-disconnect classification.
//!
//! Replaces the `is_client_disconnect_error` downcast pattern with a typed enum
//! that carries the disconnect signal through the entire handler stack without
//! any `downcast_ref` inspection at each call site.

use std::fmt;

/// Session-level error that carries the client-disconnect signal through the handler stack.
///
/// Control flow:
/// - `ClientDisconnect` — client closed connection; don't log, don't retry, propagate cleanly
/// - `Backend` — log, and retry only when the client has not already been written
///
/// A failed client write is a `Backend` error so it is not reported as a
/// disconnect. [`SessionError::blocks_another_response`] stops a later attempt
/// from writing another response onto that stream. That includes a client
/// write failure and a backend failure after response bytes were already sent.
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

impl SessionError {
    /// A client write failed after the proxy may already have sent response bytes.
    ///
    /// The attempt must stop so the next backend, a backend-error reply, or
    /// `430` is not written after it. Pool fate is separate: a preserved
    /// backend response stays reusable.
    pub(crate) fn is_failed_client_write(&self) -> bool {
        let Self::Backend(error) = self else {
            return false;
        };
        let Some(transfer) =
            error.downcast_ref::<crate::session::response_transfer::ResponseTransferError>()
        else {
            return false;
        };
        if let crate::session::response_transfer::ResponseTransferError::ClientWrite(_)
        | crate::session::response_transfer::ResponseTransferError::ClientWriteUnpreserved(_) =
            transfer
        {
            return true;
        }
        false
    }

    /// The client may already have response bytes, so another response must not
    /// be written after this error.
    pub(crate) fn blocks_another_response(&self) -> bool {
        if self.is_failed_client_write() {
            return true;
        }
        let Self::Backend(error) = self else {
            return false;
        };
        let Some(transfer) =
            error.downcast_ref::<crate::session::response_transfer::ResponseTransferError>()
        else {
            return false;
        };
        if let crate::session::response_transfer::ResponseTransferError::DeliveredPrefix(_) =
            transfer
        {
            return true;
        }
        false
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

        let e = match e.downcast::<Self>() {
            Ok(session_error) => return session_error,
            Err(error) => error,
        };

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

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::from(anyhow::Error::from(error))
    }
}

/// Convert response-transfer errors to `SessionError`, preserving the disconnect signal.
impl From<crate::session::response_transfer::ResponseTransferError> for SessionError {
    fn from(e: crate::session::response_transfer::ResponseTransferError) -> Self {
        match e {
            crate::session::response_transfer::ResponseTransferError::ClientDisconnect(io_err) => {
                Self::ClientDisconnect(io_err)
            }
            crate::session::response_transfer::ResponseTransferError::ClientWrite(io_err)
            | crate::session::response_transfer::ResponseTransferError::ClientWriteUnpreserved(
                io_err,
            ) if crate::connection_error::is_disconnect_kind(io_err.kind()) => {
                Self::ClientDisconnect(io_err)
            }
            crate::session::response_transfer::ResponseTransferError::ClientWrite(io_err) => {
                Self::Backend(anyhow::Error::new(
                    crate::session::response_transfer::ResponseTransferError::ClientWrite(io_err),
                ))
            }
            crate::session::response_transfer::ResponseTransferError::ClientWriteUnpreserved(
                io_err,
            ) => Self::Backend(anyhow::Error::new(
                crate::session::response_transfer::ResponseTransferError::ClientWriteUnpreserved(
                    io_err,
                ),
            )),
            other => Self::Backend(anyhow::Error::new(other)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
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
    fn client_disconnect_survives_anyhow_wrapping() {
        let original = SessionError::ClientDisconnect(std::io::Error::from(ErrorKind::BrokenPipe));
        let restored = SessionError::from(anyhow::Error::new(original));
        assert!(matches!(restored, SessionError::ClientDisconnect(_)));
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
    fn client_disconnect_from_response_transfer_error() {
        let io_err = std::io::Error::from(ErrorKind::BrokenPipe);
        let response_transfer_err =
            crate::session::response_transfer::ResponseTransferError::ClientDisconnect(io_err);
        let e = SessionError::from(response_transfer_err);
        assert!(matches!(e, SessionError::ClientDisconnect(_)));
    }

    #[test]
    fn backend_from_response_transfer_backend_eof() {
        let response_transfer_err =
            crate::session::response_transfer::ResponseTransferError::BackendEof {
                backend_id: crate::types::BackendId::from_index(0),
                bytes_received: 100,
            };
        let e = SessionError::from(response_transfer_err);
        assert!(matches!(e, SessionError::Backend(_)));
    }

    #[test]
    fn client_disconnect_from_response_transfer_client_write() {
        let io_err = std::io::Error::from(ErrorKind::BrokenPipe);
        let response_transfer_err =
            crate::session::response_transfer::ResponseTransferError::ClientWrite(io_err);
        let e = SessionError::from(response_transfer_err);
        assert!(matches!(e, SessionError::ClientDisconnect(_)));
    }

    #[test]
    fn timed_out_client_write_stays_a_backend_error() {
        let io_err = std::io::Error::new(ErrorKind::TimedOut, "timed out");
        let response_transfer_err =
            crate::session::response_transfer::ResponseTransferError::ClientWrite(io_err);
        let e = SessionError::from(response_transfer_err);
        assert!(e.is_failed_client_write());
        let SessionError::Backend(error) = e else {
            panic!("timed-out client write was reported as a disconnect");
        };
        let rendered = error.to_string();
        assert!(rendered.contains("client write failed"), "{rendered}");
        assert!(!rendered.contains("client disconnected"), "{rendered}");
    }

    #[test]
    fn unpreserved_client_write_stops_the_attempt() {
        let io_err = std::io::Error::new(ErrorKind::TimedOut, "timed out");
        let response_transfer_err =
            crate::session::response_transfer::ResponseTransferError::ClientWriteUnpreserved(
                io_err,
            );
        let error = SessionError::from(response_transfer_err);
        assert!(error.is_failed_client_write());
        assert!(error.blocks_another_response());
    }

    #[test]
    fn delivered_prefix_stops_another_response_without_a_client_write() {
        let eof = crate::session::response_transfer::ResponseTransferError::BackendEof {
            backend_id: crate::types::BackendId::from_index(0),
            bytes_received: 12,
        };
        let delivered = eof.after_client_delivery();
        let error = SessionError::from(delivered);
        assert!(error.blocks_another_response());
        assert!(!error.is_failed_client_write());
        let rendered = error.to_string();
        assert!(
            rendered.contains("client already received response bytes"),
            "{rendered}"
        );
    }
}
