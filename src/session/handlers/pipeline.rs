//! TCP command pipelining for per-command routing
//!
//! When a client sends multiple commands in a single TCP buffer (common with NZB
//! downloaders batching STAT/ARTICLE commands), this module reads them as a batch
//! so they can be processed without blocking on socket reads between each command.
//!
//! Single-command batches fall through to the existing sequential path with zero overhead.

use crate::protocol::RequestContext;
use crate::session::ClientSession;
use anyhow::Result;
use tokio::io::AsyncBufReadExt;

/// Maximum pipeline depth (number of commands read from client buffer at once)
const MAX_PIPELINE_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestRejection {
    Oversized { wire_len: usize },
    Invalid,
}

/// A batch of requests read from the client's TCP buffer.
///
/// Uses typed contexts for pipelineable requests and the trailing
/// non-pipelineable line, keeping one request model for the whole batch.
pub(super) struct RequestBatch {
    /// Typed contexts for each pipelineable command.
    contexts: smallvec::SmallVec<[RequestContext; 4]>,
    /// Typed context for trailing non-pipelineable command if present.
    trailing_context: Option<RequestContext>,
    /// Rejection for the first command, before context creation.
    first_rejection: Option<RequestRejection>,
    /// Rejection for a trailing command after pipelineable contexts.
    trailing_rejection: Option<RequestRejection>,
}

impl RequestBatch {
    fn empty() -> Self {
        Self {
            contexts: smallvec::SmallVec::new(),
            trailing_context: None,
            first_rejection: None,
            trailing_rejection: None,
        }
    }

    fn first_oversized() -> Self {
        Self {
            first_rejection: Some(RequestRejection::Oversized { wire_len: 0 }),
            ..Self::empty()
        }
    }

    fn first_invalid() -> Self {
        Self {
            first_rejection: Some(RequestRejection::Invalid),
            ..Self::empty()
        }
    }

    fn trailing(context: RequestContext) -> Self {
        Self {
            trailing_context: Some(context),
            ..Self::empty()
        }
    }

    const fn contexts_with_trailing_oversized(
        contexts: smallvec::SmallVec<[RequestContext; 4]>,
        trailing_wire_len: usize,
    ) -> Self {
        Self {
            contexts,
            trailing_context: None,
            first_rejection: None,
            trailing_rejection: Some(RequestRejection::Oversized {
                wire_len: trailing_wire_len,
            }),
        }
    }

    const fn contexts_with_trailing_invalid(
        contexts: smallvec::SmallVec<[RequestContext; 4]>,
    ) -> Self {
        Self {
            contexts,
            trailing_context: None,
            first_rejection: None,
            trailing_rejection: Some(RequestRejection::Invalid),
        }
    }

    const fn contexts_with_trailing(
        contexts: smallvec::SmallVec<[RequestContext; 4]>,
        trailing_context: RequestContext,
    ) -> Self {
        Self {
            contexts,
            trailing_context: Some(trailing_context),
            first_rejection: None,
            trailing_rejection: None,
        }
    }

    fn contexts(contexts: smallvec::SmallVec<[RequestContext; 4]>) -> Self {
        Self {
            contexts,
            ..Self::empty()
        }
    }

    /// Whether this batch is empty (client disconnected)
    pub(super) fn is_empty(&self) -> bool {
        self.contexts.is_empty()
            && self.trailing_context.is_none()
            && self.first_rejection.is_none()
            && self.trailing_rejection.is_none()
    }

    /// Get a typed context by index from the pipelineable commands.
    pub(super) fn context(&self, i: usize) -> &RequestContext {
        &self.contexts[i]
    }

    /// Get a mutable typed context by index from the pipelineable commands.
    pub(super) fn context_mut(&mut self, i: usize) -> &mut RequestContext {
        &mut self.contexts[i]
    }

    /// Get the trailing typed context if present.
    pub(super) const fn trailing_context(&self) -> Option<&RequestContext> {
        self.trailing_context.as_ref()
    }

    /// Get the trailing typed context mutably if present.
    pub(super) const fn trailing_context_mut(&mut self) -> Option<&mut RequestContext> {
        self.trailing_context.as_mut()
    }

    /// Number of pipelineable commands
    pub(super) fn len(&self) -> usize {
        self.contexts.len()
    }

    /// Whether the trailing command exceeded the 512-byte RFC 3977 limit
    pub const fn is_trailing_oversized(&self) -> bool {
        matches!(
            self.trailing_rejection,
            Some(RequestRejection::Oversized { .. })
        )
    }

    /// Whether the trailing command was syntactically invalid.
    pub const fn is_trailing_invalid(&self) -> bool {
        matches!(self.trailing_rejection, Some(RequestRejection::Invalid))
    }

    /// Wire length for the trailing oversized command, if any.
    pub const fn trailing_wire_len(&self) -> usize {
        match self.trailing_rejection {
            Some(RequestRejection::Oversized { wire_len }) => wire_len,
            Some(RequestRejection::Invalid) | None => 0,
        }
    }

    /// Whether the *first* command (blocking read) exceeded the 512-byte limit.
    /// When true, the batch is otherwise empty — caller should send 501 and continue.
    pub const fn is_first_oversized(&self) -> bool {
        matches!(
            self.first_rejection,
            Some(RequestRejection::Oversized { .. })
        )
    }

    /// Whether the first command was syntactically invalid.
    pub const fn is_first_invalid(&self) -> bool {
        matches!(self.first_rejection, Some(RequestRejection::Invalid))
    }
}

impl ClientSession {
    /// Read a batch of commands from the client's buffered reader.
    ///
    /// The first command always blocks (waiting for client input). Subsequent
    /// commands are read non-blocking from the `BufReader`'s userspace buffer —
    /// if data is already available, it's consumed; otherwise the batch ends.
    ///
    /// Returns empty batch on client disconnect.
    ///
    pub(super) async fn read_command_batch<R>(
        &self,
        reader: &mut tokio::io::BufReader<R>,
        command_buf: &mut Vec<u8>,
    ) -> Result<RequestBatch>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        // First command: blocking read (must wait for client)
        command_buf.clear();
        match reader.read_until(b'\n', command_buf).await {
            Ok(0) => {
                return Ok(RequestBatch::empty());
            }
            Ok(_) => {
                // RFC 3977 §3.1: 512-byte command limit — return 501 and keep session alive
                if command_buf.len() > crate::protocol::MAX_COMMAND_LINE_OCTETS {
                    return Ok(RequestBatch::first_oversized());
                }
            }
            Err(e) => return Err(e.into()),
        }

        let Some(request) = RequestContext::parse(command_buf) else {
            return Ok(RequestBatch::first_invalid());
        };
        if !request.is_pipelineable() {
            // Single non-pipelineable command → return as trailing
            return Ok(RequestBatch::trailing(request));
        }

        let mut batch_contexts: smallvec::SmallVec<[RequestContext; 4]> = smallvec::SmallVec::new();
        batch_contexts.push(request);

        // Read more commands from the buffer (non-blocking)
        while batch_contexts.len() < MAX_PIPELINE_DEPTH {
            // Only proceed if buffer has a complete line (contains \n).
            // Checking just is_empty() is insufficient: if the buffer has a partial
            // command without \n, read_until() would block on the socket waiting for
            // more data, defeating the non-blocking batch intent.
            if memchr::memchr(b'\n', reader.buffer()).is_none() {
                break;
            }

            command_buf.clear();
            match reader.read_until(b'\n', command_buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // M4: Reject oversized commands (end batch on invalid command)
                    // Mark as oversized so caller sends 500 error instead of forwarding
                    if command_buf.len() > crate::protocol::MAX_COMMAND_LINE_OCTETS {
                        return Ok(RequestBatch::contexts_with_trailing_oversized(
                            batch_contexts,
                            command_buf.len(),
                        ));
                    }
                    let Some(request) = RequestContext::parse(command_buf) else {
                        return Ok(RequestBatch::contexts_with_trailing_invalid(batch_contexts));
                    };
                    if !request.is_pipelineable() {
                        // Non-pipelineable command ends the batch
                        return Ok(RequestBatch::contexts_with_trailing(
                            batch_contexts,
                            request,
                        ));
                    }
                    batch_contexts.push(request);
                }
            }
        }

        Ok(RequestBatch::contexts(batch_contexts))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncWriteExt, BufReader};

    use crate::auth::AuthHandler;
    use crate::constants::buffer::COMMAND;
    use crate::metrics::MetricsCollector;
    use crate::pool::BufferPool;
    use crate::protocol::{RequestKind, RequestRouteClass};
    use crate::session::ClientSession;
    use crate::types::{BufferSize, ClientAddress};

    fn test_session() -> ClientSession {
        let addr: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let metrics = MetricsCollector::new(1);
        ClientSession::builder(
            ClientAddress::from(addr),
            buffer_pool,
            auth_handler,
            metrics,
        )
        .build()
    }

    #[tokio::test]
    async fn read_command_batch_preserves_non_utf8_trailing_command_bytes() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client
            .write_all(b"ARTICLE <a@b>\r\nXFOO \xff\r\n")
            .await
            .unwrap();
        drop(client);

        let mut reader = BufReader::new(server);
        let mut command_buf = Vec::with_capacity(COMMAND);

        let batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.context(0).kind(), RequestKind::Article);
        let trailing = batch
            .trailing_context()
            .expect("non-pipelineable command trails the ARTICLE batch");
        assert_eq!(trailing.kind(), RequestKind::Unknown);
        assert_eq!(trailing.args(), b"\xff");
        assert_eq!(trailing.route_class(), RequestRouteClass::Stateful);
    }

    #[tokio::test]
    async fn read_command_batch_rejects_oversized_trailing_before_context_creation() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        let oversized_arg = vec![b'a'; 520];

        client.write_all(b"STAT <a@b>\r\nXOVER ").await.unwrap();
        client.write_all(&oversized_arg).await.unwrap();
        client.write_all(b"\r\n").await.unwrap();
        drop(client);

        let mut reader = BufReader::new(server);
        let mut command_buf = Vec::with_capacity(COMMAND);

        let batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.context(0).kind(), RequestKind::Stat);
        assert!(batch.is_trailing_oversized());
        assert!(batch.trailing_context().is_none());
        assert!(batch.trailing_wire_len() > 512);
    }

    #[tokio::test]
    async fn read_command_batch_rejects_empty_first_line_before_context_creation() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(b"\r\n").await.unwrap();
        drop(client);

        let mut reader = BufReader::new(server);
        let mut command_buf = Vec::with_capacity(COMMAND);

        let batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();

        assert!(batch.is_first_invalid());
        assert_eq!(batch.len(), 0);
        assert!(batch.trailing_context().is_none());
    }

    #[tokio::test]
    async fn read_command_batch_rejects_empty_trailing_line_before_context_creation() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(b"STAT <a@b>\r\n\r\n").await.unwrap();
        drop(client);

        let mut reader = BufReader::new(server);
        let mut command_buf = Vec::with_capacity(COMMAND);

        let batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.context(0).kind(), RequestKind::Stat);
        assert!(batch.is_trailing_invalid());
        assert!(batch.trailing_context().is_none());
    }
}
