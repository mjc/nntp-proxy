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

/// A batch of requests read from the client's TCP buffer.
///
/// Uses typed contexts for pipelineable requests and the trailing
/// non-pipelineable line, avoiding parallel raw command state.
pub(super) struct RequestBatch {
    /// Typed contexts for each pipelineable command.
    contexts: smallvec::SmallVec<[RequestContext; 4]>,
    /// Typed context for trailing non-pipelineable command if present.
    trailing_context: Option<RequestContext>,
    /// True if the trailing command exceeded the 512-byte RFC 3977 limit
    trailing_oversized: bool,
    /// True if the first (blocking) command exceeded the 512-byte RFC 3977 limit.
    /// The batch is otherwise empty; caller must send 501 and continue.
    first_oversized: bool,
}

impl RequestBatch {
    /// Whether this batch is empty (client disconnected)
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty() && self.trailing_context.is_none()
    }

    /// Get a typed context by index from the pipelineable commands.
    pub fn context(&self, i: usize) -> &RequestContext {
        &self.contexts[i]
    }

    /// Get a mutable typed context by index from the pipelineable commands.
    pub fn context_mut(&mut self, i: usize) -> &mut RequestContext {
        &mut self.contexts[i]
    }

    /// Get the trailing typed context if present.
    pub fn trailing_context(&self) -> Option<&RequestContext> {
        self.trailing_context.as_ref()
    }

    /// Get the trailing typed context mutably if present.
    pub fn trailing_context_mut(&mut self) -> Option<&mut RequestContext> {
        self.trailing_context.as_mut()
    }

    /// Number of pipelineable commands
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    /// Whether the trailing command exceeded the 512-byte RFC 3977 limit
    pub const fn is_trailing_oversized(&self) -> bool {
        self.trailing_oversized
    }

    /// Whether the *first* command (blocking read) exceeded the 512-byte limit.
    /// When true, the batch is otherwise empty — caller should send 501 and continue.
    pub const fn is_first_oversized(&self) -> bool {
        self.first_oversized
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
        let mut trailing_oversized = false;

        // First command: blocking read (must wait for client)
        command_buf.clear();
        match reader.read_until(b'\n', command_buf).await {
            Ok(0) => {
                return Ok(RequestBatch {
                    contexts: smallvec::SmallVec::new(),
                    trailing_context: None,
                    trailing_oversized: false,
                    first_oversized: false,
                });
            }
            Ok(_) => {
                // RFC 3977 §3.1: 512-byte command limit — return 501 and keep session alive
                if command_buf.len() > 512 {
                    return Ok(RequestBatch {
                        contexts: smallvec::SmallVec::new(),
                        trailing_context: None,
                        trailing_oversized: false,
                        first_oversized: true,
                    });
                }
            }
            Err(e) => return Err(e.into()),
        }

        let request = RequestContext::from_request_bytes(command_buf);

        if !request.is_pipelineable() {
            // Single non-pipelineable command → return as trailing
            let trailing_context = Some(request);
            return Ok(RequestBatch {
                contexts: smallvec::SmallVec::new(),
                trailing_context,
                trailing_oversized: false,
                first_oversized: false,
            });
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
                    if command_buf.len() > 512 {
                        let trailing_context =
                            Some(RequestContext::from_request_bytes(command_buf));
                        trailing_oversized = true;
                        return Ok(RequestBatch {
                            contexts: batch_contexts,
                            trailing_context,
                            trailing_oversized,
                            first_oversized: false,
                        });
                    }
                    let request = RequestContext::from_request_bytes(command_buf);
                    if !request.is_pipelineable() {
                        // Non-pipelineable command ends the batch
                        let trailing_context = Some(request);
                        return Ok(RequestBatch {
                            contexts: batch_contexts,
                            trailing_context,
                            trailing_oversized,
                            first_oversized: false,
                        });
                    }
                    batch_contexts.push(request);
                }
            }
        }

        Ok(RequestBatch {
            contexts: batch_contexts,
            trailing_context: None,
            trailing_oversized,
            first_oversized: false,
        })
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
        assert_eq!(trailing.args_str(), None);
        assert_eq!(trailing.route_class(), RequestRouteClass::Stateful);
    }
}
