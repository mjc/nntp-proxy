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

    /// Client write failed after backend response ownership was already established.
    ///
    /// This is treated as terminal for backend-connection reuse to prevent
    /// protocol desync when a partially-written client response races with
    /// subsequent backend reuse.
    ClientWrite(std::io::Error),

    /// Backend closed connection before sending a complete multiline response.
    BackendEof {
        backend_id: crate::types::BackendId,
        bytes_received: u64,
    },

    /// Other I/O / protocol error.
    Io(anyhow::Error),
}

/// Fate of a backend connection after a response transfer.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendConnectionOutcome {
    BackendHealthy,
    BackendDirty,
    BackendFailed,
}

impl ResponseTransferError {
    /// Derive the backend connection fate from the response-transfer outcome.
    ///
    /// A plain client disconnect after a complete response has been captured is
    /// safe for connection reuse. Backend EOF and other I/O errors may leave
    /// unread bytes in flight and must retire it. If the connection still has
    /// queued bytes after the transfer, it is dirty even when the transfer
    /// itself only saw a client disconnect.
    pub(super) const fn pool_fate(
        &self,
        reuse: ResponseConnectionReuse,
    ) -> BackendConnectionOutcome {
        match reuse {
            ResponseConnectionReuse::QueuedBytes { .. } => BackendConnectionOutcome::BackendDirty,
            ResponseConnectionReuse::Reusable => match self {
                Self::ClientDisconnect(_) => BackendConnectionOutcome::BackendHealthy,
                Self::ClientWrite(_) | Self::BackendEof { .. } | Self::Io(_) => {
                    BackendConnectionOutcome::BackendFailed
                }
            },
        }
    }
}

impl std::fmt::Display for ResponseTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientDisconnect(e) => write!(f, "client disconnected: {e}"),
            Self::ClientWrite(e) => write!(f, "client write failed: {e}"),
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
            Self::ClientWrite(e) => Some(e),
            Self::BackendEof { .. } => None,
            Self::Io(e) => e.source(),
        }
    }
}

/// Reuse decision after a response transfer completed successfully.
#[derive(Debug, Clone, Copy)]
pub(super) enum ResponseConnectionReuse {
    /// No queued bytes remain on the connection.
    Reusable,
    /// Bytes are queued for the next response and the connection must stay on
    /// the same processing path until they are consumed.
    QueuedBytes { len: usize },
}

impl ResponseConnectionReuse {
    /// Derive the backend connection fate from a successful transfer.
    pub(super) const fn pool_fate(self) -> BackendConnectionOutcome {
        match self {
            Self::Reusable => BackendConnectionOutcome::BackendHealthy,
            Self::QueuedBytes { .. } => BackendConnectionOutcome::BackendDirty,
        }
    }
}

/// Inspect a connection after response transfer without exposing the queued
/// bytes themselves to the caller.
#[must_use]
pub(super) fn connection_reuse_after_response(
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
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn response_transfer_error_pool_fate_matches_disconnect_semantics() {
        let disconnect = ResponseTransferError::ClientDisconnect(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        ));
        let client_write = ResponseTransferError::ClientWrite(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        ));
        let eof = ResponseTransferError::BackendEof {
            backend_id: crate::types::BackendId::from_index(0),
            bytes_received: 12,
        };
        let io = ResponseTransferError::Io(anyhow::anyhow!("backend dirty"));

        assert_eq!(
            disconnect.pool_fate(ResponseConnectionReuse::Reusable),
            BackendConnectionOutcome::BackendHealthy
        );
        assert_eq!(
            disconnect.pool_fate(ResponseConnectionReuse::QueuedBytes { len: 1 }),
            BackendConnectionOutcome::BackendDirty
        );
        assert_eq!(
            client_write.pool_fate(ResponseConnectionReuse::Reusable),
            BackendConnectionOutcome::BackendFailed
        );
        assert_eq!(
            eof.pool_fate(ResponseConnectionReuse::Reusable),
            BackendConnectionOutcome::BackendFailed
        );
        assert_eq!(
            io.pool_fate(ResponseConnectionReuse::Reusable),
            BackendConnectionOutcome::BackendFailed
        );
        assert_eq!(
            ResponseConnectionReuse::Reusable.pool_fate(),
            BackendConnectionOutcome::BackendHealthy
        );
        assert_eq!(
            ResponseConnectionReuse::QueuedBytes { len: 12 }.pool_fate(),
            BackendConnectionOutcome::BackendDirty
        );
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
}
