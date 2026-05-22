//! Response transfer error and connection reuse decisions.
//!
//! This module deliberately contains no response framing logic. It records the
//! outcome of a transfer after the framer/backend layer has already determined
//! whether a response was completely consumed, partially consumed, or left the
//! backend connection with queued bytes.

/// Outcome of transferring a backend response.
#[derive(Debug)]
pub(crate) enum ResponseTransferError {
    /// Client disconnected after the proxy had already captured a complete
    /// backend response and started writing it locally.
    ClientDisconnect(std::io::Error),

    /// Backend closed connection before sending a complete multiline response.
    BackendEof {
        backend_id: crate::types::BackendId,
        bytes_received: u64,
    },

    /// Other I/O / protocol error.
    Io(anyhow::Error),
}

impl ResponseTransferError {
    /// Whether this failure leaves the backend connection unusable.
    ///
    /// A plain client disconnect after a complete response has been captured is
    /// safe for connection reuse. Backend EOF and other I/O errors may leave
    /// unread bytes in flight and must retire it.
    pub(crate) const fn must_remove_connection(&self) -> bool {
        !matches!(self, Self::ClientDisconnect(_))
    }

    /// Convert the transfer outcome into an `anyhow` error for older call sites
    /// that still bubble untyped errors.
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::ClientDisconnect(io_err) => anyhow::Error::from(io_err),
            Self::Io(e) => e,
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => anyhow::anyhow!(
                "Backend {backend_id:?} closed connection before complete multiline response \
                 ({bytes_received} bytes received)"
            ),
        }
    }
}

impl std::fmt::Display for ResponseTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientDisconnect(e) => write!(f, "client disconnected: {e}"),
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => write!(
                f,
                "backend {backend_id:?} closed connection before complete multiline response \
                 ({bytes_received} bytes received)"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResponseTransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientDisconnect(e) => Some(e),
            Self::BackendEof { .. } => None,
            Self::Io(e) => e.source(),
        }
    }
}

/// Reuse decision after a response transfer completed successfully.
pub(crate) enum ResponseConnectionReuse {
    /// No queued bytes remain on the connection.
    Reusable,
    /// Bytes are queued for the next response and the connection must stay on
    /// the same processing path until they are consumed.
    QueuedBytes { len: usize },
}

/// Inspect a connection after response transfer without exposing the queued
/// bytes themselves to the caller.
#[must_use]
pub(crate) fn connection_reuse_after_response(
    conn: &crate::pool::ConnectionGuard,
) -> ResponseConnectionReuse {
    if conn.has_pending_bytes() {
        ResponseConnectionReuse::QueuedBytes {
            len: conn.pending_bytes_len(),
        }
    } else {
        ResponseConnectionReuse::Reusable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_transfer_error_pool_fate_matches_disconnect_semantics() {
        let disconnect = ResponseTransferError::ClientDisconnect(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        ));
        let eof = ResponseTransferError::BackendEof {
            backend_id: crate::types::BackendId::from_index(0),
            bytes_received: 12,
        };
        let io = ResponseTransferError::Io(anyhow::anyhow!("backend dirty"));

        assert!(!disconnect.must_remove_connection());
        assert!(eof.must_remove_connection());
        assert!(io.must_remove_connection());
    }

    #[test]
    fn response_transfer_error_preserves_sources_for_io_variants() {
        let disconnect = ResponseTransferError::ClientDisconnect(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        ));
        assert!(std::error::Error::source(&disconnect).is_some());

        let backend_eof = ResponseTransferError::BackendEof {
            backend_id: crate::types::BackendId::from_index(2),
            bytes_received: 99,
        };
        assert!(std::error::Error::source(&backend_eof).is_none());
        assert!(
            backend_eof.to_string().contains("99 bytes received"),
            "display should include received byte count"
        );
    }

    #[test]
    fn response_transfer_into_anyhow_keeps_backend_eof_context() {
        let err = ResponseTransferError::BackendEof {
            backend_id: crate::types::BackendId::from_index(3),
            bytes_received: 7,
        }
        .into_anyhow();

        let rendered = err.to_string();
        assert!(rendered.contains("closed connection"));
        assert!(rendered.contains("7 bytes received"));
    }
}
