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
use smallvec::SmallVec;
use tokio::io::AsyncBufReadExt;
use tokio::time::Duration;

/// Maximum pipeline depth (number of commands read from client buffer at once)
const MAX_PIPELINE_DEPTH: usize = 16;
const COMMAND_LINE_CAPACITY: usize = crate::protocol::MAX_COMMAND_LINE_OCTETS;
const PIPELINE_REFILL_GRACE: Duration = Duration::from_millis(1);
type BatchContexts = SmallVec<[RequestContext; MAX_PIPELINE_DEPTH]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandLineKind {
    Eof,
    Oversized { wire_len: usize },
    Parsed,
}

struct CommandLine {
    kind: CommandLineKind,
    request: Option<RequestContext>,
}

impl CommandLine {
    const fn eof() -> Self {
        Self {
            kind: CommandLineKind::Eof,
            request: None,
        }
    }

    const fn oversized(wire_len: usize) -> Self {
        Self {
            kind: CommandLineKind::Oversized { wire_len },
            request: None,
        }
    }

    const fn parsed(request: Option<RequestContext>) -> Self {
        Self {
            kind: CommandLineKind::Parsed,
            request,
        }
    }
}

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
    contexts: BatchContexts,
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
        contexts: BatchContexts,
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

    const fn contexts_with_trailing_invalid(contexts: BatchContexts) -> Self {
        Self {
            contexts,
            trailing_context: None,
            first_rejection: None,
            trailing_rejection: Some(RequestRejection::Invalid),
        }
    }

    const fn contexts_with_trailing(
        contexts: BatchContexts,
        trailing_context: RequestContext,
    ) -> Self {
        Self {
            contexts,
            trailing_context: Some(trailing_context),
            first_rejection: None,
            trailing_rejection: None,
        }
    }

    fn contexts(contexts: BatchContexts) -> Self {
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
    async fn read_command_line<R>(
        reader: &mut tokio::io::BufReader<R>,
        line_buf: &mut [u8; COMMAND_LINE_CAPACITY],
    ) -> std::io::Result<CommandLine>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(CommandLine::eof());
        }
        if let Some(pos) = memchr::memchr(b'\n', available) {
            let wire_len = pos + 1;
            if wire_len > crate::protocol::MAX_COMMAND_LINE_OCTETS {
                reader.consume(wire_len);
                return Ok(CommandLine::oversized(wire_len));
            }
            let request = RequestContext::parse(&available[..wire_len]);
            reader.consume(wire_len);
            return Ok(CommandLine::parsed(request));
        }

        let mut len = 0usize;
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(if len == 0 {
                    CommandLine::eof()
                } else {
                    CommandLine::parsed(RequestContext::parse(&line_buf[..len]))
                });
            }

            let newline = memchr::memchr(b'\n', available);
            let take = newline.map_or(available.len(), |pos| pos + 1);
            if len + take > crate::protocol::MAX_COMMAND_LINE_OCTETS {
                reader.consume(take);
                if newline.is_some() {
                    return Ok(CommandLine::oversized(len + take));
                }
                return Self::drain_oversized_command_line(reader, len + take).await;
            }

            line_buf[len..len + take].copy_from_slice(&available[..take]);
            reader.consume(take);
            len += take;

            if newline.is_some() {
                return Ok(CommandLine::parsed(RequestContext::parse(&line_buf[..len])));
            }
        }
    }

    async fn drain_oversized_command_line<R>(
        reader: &mut tokio::io::BufReader<R>,
        mut wire_len: usize,
    ) -> std::io::Result<CommandLine>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(CommandLine::oversized(wire_len));
            }

            let newline = memchr::memchr(b'\n', available);
            let take = newline.map_or(available.len(), |pos| pos + 1);
            reader.consume(take);
            wire_len += take;

            if newline.is_some() {
                return Ok(CommandLine::oversized(wire_len));
            }
        }
    }

    fn queued_complete_command_line<R>(reader: &tokio::io::BufReader<R>) -> bool
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        memchr::memchr(b'\n', reader.buffer()).is_some()
    }

    async fn refill_available_client_bytes<R>(
        reader: &mut tokio::io::BufReader<R>,
    ) -> std::io::Result<bool>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        if !reader.buffer().is_empty() {
            return Ok(false);
        }

        match tokio::time::timeout(PIPELINE_REFILL_GRACE, reader.fill_buf()).await {
            Ok(result) => result.map(|buf| !buf.is_empty()),
            Err(_) => Ok(false),
        }
    }

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
        line_buf: &mut [u8; COMMAND_LINE_CAPACITY],
    ) -> Result<RequestBatch>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        // First command: blocking read (must wait for client)
        let line = Self::read_command_line(reader, line_buf).await?;
        let request = match line.kind {
            CommandLineKind::Eof => return Ok(RequestBatch::empty()),
            CommandLineKind::Parsed => line.request,
            CommandLineKind::Oversized { .. } => return Ok(RequestBatch::first_oversized()),
        };

        let Some(request) = request else {
            return Ok(RequestBatch::first_invalid());
        };
        if !request.is_pipelineable() {
            // Single non-pipelineable command → return as trailing
            return Ok(RequestBatch::trailing(request));
        }

        let mut batch_contexts = BatchContexts::new();
        batch_contexts.push(request);

        // Read more commands from the buffer (non-blocking)
        while batch_contexts.len() < MAX_PIPELINE_DEPTH {
            if !Self::queued_complete_command_line(reader)
                && !Self::refill_available_client_bytes(reader).await?
            {
                break;
            }
            // Only proceed if the buffer has a complete line. If a nonblocking
            // refill found only a partial command, stop the batch so the next
            // outer-loop read can wait for the command to complete.
            if !Self::queued_complete_command_line(reader) {
                break;
            }

            let line = Self::read_command_line(reader, line_buf).await?;
            match line.kind {
                CommandLineKind::Eof => break,
                CommandLineKind::Oversized { wire_len } => {
                    return Ok(RequestBatch::contexts_with_trailing_oversized(
                        batch_contexts,
                        wire_len,
                    ));
                }
                CommandLineKind::Parsed => {
                    let Some(request) = line.request else {
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

    const fn command_line_buf() -> [u8; super::COMMAND_LINE_CAPACITY] {
        [0; super::COMMAND_LINE_CAPACITY]
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
        let mut command_buf = command_line_buf();

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
        let mut command_buf = command_line_buf();

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
        let mut command_buf = command_line_buf();

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
        let mut command_buf = command_line_buf();

        let batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.context(0).kind(), RequestKind::Stat);
        assert!(batch.is_trailing_invalid());
        assert!(batch.trailing_context().is_none());
    }

    #[tokio::test]
    async fn read_command_batch_preserves_trailing_group_after_four_body_commands() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client
            .write_all(
                concat!(
                    "BODY <body-1@example>\r\n",
                    "BODY <body-2@example>\r\n",
                    "BODY <body-3@example>\r\n",
                    "BODY <body-4@example>\r\n",
                    "GROUP alt.test\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(client);

        let mut reader = BufReader::new(server);
        let mut command_buf = command_line_buf();

        let batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();

        assert_eq!(batch.len(), 4);
        for idx in 0..4 {
            assert_eq!(batch.context(idx).kind(), RequestKind::Body);
        }
        let trailing = batch
            .trailing_context()
            .expect("GROUP should be preserved as the trailing stateful command");
        assert_eq!(trailing.kind(), RequestKind::Group);
        assert_eq!(trailing.args(), b"alt.test");
        assert_eq!(trailing.route_class(), RequestRouteClass::Stateful);
    }

    #[tokio::test]
    async fn read_command_batch_reads_second_queued_body_burst_after_first_batch() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client
            .write_all(
                concat!(
                    "BODY <body-1@example>\r\n",
                    "BODY <body-2@example>\r\n",
                    "BODY <body-3@example>\r\n",
                    "BODY <body-4@example>\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut reader = BufReader::new(server);
        let mut command_buf = command_line_buf();

        let first_batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();
        assert_eq!(first_batch.len(), 4);
        for idx in 0..4 {
            assert_eq!(first_batch.context(idx).kind(), RequestKind::Body);
            assert_eq!(
                first_batch.context(idx).args(),
                format!("<body-{}@example>", idx + 1).as_bytes()
            );
        }
        assert!(first_batch.trailing_context().is_none());

        client
            .write_all(
                concat!(
                    "BODY <body-5@example>\r\n",
                    "BODY <body-6@example>\r\n",
                    "BODY <body-7@example>\r\n",
                    "BODY <body-8@example>\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(client);

        let second_batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();
        assert_eq!(second_batch.len(), 4);
        for idx in 0..4 {
            assert_eq!(second_batch.context(idx).kind(), RequestKind::Body);
            assert_eq!(
                second_batch.context(idx).args(),
                format!("<body-{}@example>", idx + 5).as_bytes()
            );
        }
        assert!(second_batch.trailing_context().is_none());
    }

    #[tokio::test]
    async fn read_command_batch_resumes_partial_second_body_burst_before_trailing_group() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client
            .write_all(
                concat!(
                    "BODY <body-1@example>\r\n",
                    "BODY <body-2@example>\r\n",
                    "BODY <body-3@example>\r\n",
                    "BODY <body-4@example>\r\n",
                    "BODY <body-5@examp",
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut reader = BufReader::new(server);
        let mut command_buf = command_line_buf();

        let first_batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();
        assert_eq!(first_batch.len(), 4);
        for idx in 0..4 {
            assert_eq!(first_batch.context(idx).kind(), RequestKind::Body);
            assert_eq!(
                first_batch.context(idx).args(),
                format!("<body-{}@example>", idx + 1).as_bytes()
            );
        }
        assert!(first_batch.trailing_context().is_none());

        client
            .write_all(b"le>\r\nGROUP alt.test\r\n")
            .await
            .unwrap();
        drop(client);

        let second_batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch.context(0).kind(), RequestKind::Body);
        assert_eq!(second_batch.context(0).args(), b"<body-5@example>");

        let trailing = second_batch
            .trailing_context()
            .expect("GROUP should remain trailing after resuming the partial BODY");
        assert_eq!(trailing.kind(), RequestKind::Group);
        assert_eq!(trailing.args(), b"alt.test");
        assert_eq!(trailing.route_class(), RequestRouteClass::Stateful);
    }

    #[tokio::test]
    async fn read_command_batch_reads_second_queued_article_burst_with_trailing_group() {
        let session = test_session();
        let (mut client, server) = tokio::io::duplex(4096);
        client
            .write_all(
                concat!(
                    "ARTICLE <article-1@example>\r\n",
                    "ARTICLE <article-2@example>\r\n",
                    "ARTICLE <article-3@example>\r\n",
                    "ARTICLE <article-4@example>\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut reader = BufReader::new(server);
        let mut command_buf = command_line_buf();

        let first_batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();
        assert_eq!(first_batch.len(), 4);
        for idx in 0..4 {
            assert_eq!(first_batch.context(idx).kind(), RequestKind::Article);
            assert_eq!(
                first_batch.context(idx).args(),
                format!("<article-{}@example>", idx + 1).as_bytes()
            );
        }
        assert!(first_batch.trailing_context().is_none());

        client
            .write_all(
                concat!(
                    "ARTICLE <article-5@example>\r\n",
                    "ARTICLE <article-6@example>\r\n",
                    "ARTICLE <article-7@example>\r\n",
                    "ARTICLE <article-8@example>\r\n",
                    "GROUP alt.test\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(client);

        let second_batch = session
            .read_command_batch(&mut reader, &mut command_buf)
            .await
            .unwrap();
        assert_eq!(second_batch.len(), 4);
        for idx in 0..4 {
            assert_eq!(second_batch.context(idx).kind(), RequestKind::Article);
            assert_eq!(
                second_batch.context(idx).args(),
                format!("<article-{}@example>", idx + 5).as_bytes()
            );
        }
        let trailing = second_batch
            .trailing_context()
            .expect("GROUP should remain trailing after the second ARTICLE burst");
        assert_eq!(trailing.kind(), RequestKind::Group);
        assert_eq!(trailing.args(), b"alt.test");
        assert_eq!(trailing.route_class(), RequestRouteClass::Stateful);
    }
}
