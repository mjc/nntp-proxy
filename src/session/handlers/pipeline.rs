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

#[expect(
    clippy::large_enum_variant,
    reason = "retain inline request storage without per-command allocation"
)]
#[derive(Debug)]
enum CommandLine {
    Eof,
    Oversized { wire_len: usize },
    Invalid,
    Parsed(RequestContext),
}

impl CommandLine {
    const fn eof() -> Self {
        Self::Eof
    }

    const fn oversized(wire_len: usize) -> Self {
        Self::Oversized { wire_len }
    }

    fn parsed(request: Option<RequestContext>) -> Self {
        request.map_or(Self::Invalid, Self::Parsed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestRejection {
    Oversized { wire_len: usize },
    Invalid,
}

#[expect(
    clippy::large_enum_variant,
    reason = "retain the inline pipeline capacity"
)]
#[derive(Debug)]
enum BatchContents {
    Empty,
    Rejected(RequestRejection),
    Contexts(BatchContexts),
}

#[expect(
    clippy::large_enum_variant,
    reason = "retain inline trailing request storage"
)]
#[derive(Debug)]
enum BatchTail {
    None,
    Context(RequestContext),
    Rejection(RequestRejection),
}

/// A batch of requests read from the client's TCP buffer.
///
/// The contents and trailing item are payload-bearing states, so a request and
/// its rejection cannot coexist in the same slot.
pub(super) struct RequestBatch {
    contents: BatchContents,
    trailing: BatchTail,
}

impl RequestBatch {
    fn empty() -> Self {
        Self {
            contents: BatchContents::Empty,
            trailing: BatchTail::None,
        }
    }

    fn first_oversized(wire_len: usize) -> Self {
        Self {
            contents: BatchContents::Rejected(RequestRejection::Oversized { wire_len }),
            trailing: BatchTail::None,
        }
    }

    fn first_invalid() -> Self {
        Self {
            contents: BatchContents::Rejected(RequestRejection::Invalid),
            trailing: BatchTail::None,
        }
    }

    fn trailing(context: RequestContext) -> Self {
        Self {
            contents: BatchContents::Empty,
            trailing: BatchTail::Context(context),
        }
    }

    fn contexts_with_trailing_oversized(contexts: BatchContexts, trailing_wire_len: usize) -> Self {
        Self {
            contents: BatchContents::Contexts(contexts),
            trailing: BatchTail::Rejection(RequestRejection::Oversized {
                wire_len: trailing_wire_len,
            }),
        }
    }

    fn contexts_with_trailing_invalid(contexts: BatchContexts) -> Self {
        Self {
            contents: BatchContents::Contexts(contexts),
            trailing: BatchTail::Rejection(RequestRejection::Invalid),
        }
    }

    fn contexts_with_trailing(contexts: BatchContexts, trailing_context: RequestContext) -> Self {
        Self {
            contents: BatchContents::Contexts(contexts),
            trailing: BatchTail::Context(trailing_context),
        }
    }

    fn contexts(contexts: BatchContexts) -> Self {
        Self {
            contents: BatchContents::Contexts(contexts),
            trailing: BatchTail::None,
        }
    }

    /// Whether this batch is empty (client disconnected)
    pub(super) fn is_empty(&self) -> bool {
        matches!(
            (&self.contents, &self.trailing),
            (BatchContents::Empty, BatchTail::None)
        )
    }

    /// Get a typed context by index from the pipelineable commands.
    pub(super) fn context(&self, i: usize) -> &RequestContext {
        match &self.contents {
            BatchContents::Contexts(contexts) => &contexts[i],
            BatchContents::Empty | BatchContents::Rejected(_) => {
                panic!("request context requested from a non-context batch")
            }
        }
    }

    /// Get a mutable typed context by index from the pipelineable commands.
    pub(super) fn context_mut(&mut self, i: usize) -> &mut RequestContext {
        match &mut self.contents {
            BatchContents::Contexts(contexts) => &mut contexts[i],
            BatchContents::Empty | BatchContents::Rejected(_) => {
                panic!("request context requested from a non-context batch")
            }
        }
    }

    /// Get the trailing typed context if present.
    pub(super) fn trailing_context(&self) -> Option<&RequestContext> {
        match &self.trailing {
            BatchTail::Context(context) => Some(context),
            BatchTail::None | BatchTail::Rejection(_) => None,
        }
    }

    /// Get the trailing typed context mutably if present.
    pub(super) fn trailing_context_mut(&mut self) -> Option<&mut RequestContext> {
        match &mut self.trailing {
            BatchTail::Context(context) => Some(context),
            BatchTail::None | BatchTail::Rejection(_) => None,
        }
    }

    /// Number of pipelineable commands
    pub(super) fn len(&self) -> usize {
        match &self.contents {
            BatchContents::Contexts(contexts) => contexts.len(),
            BatchContents::Empty | BatchContents::Rejected(_) => 0,
        }
    }

    /// Whether the trailing command exceeded the 512-byte RFC 3977 limit
    pub const fn is_trailing_oversized(&self) -> bool {
        matches!(
            self.trailing,
            BatchTail::Rejection(RequestRejection::Oversized { .. })
        )
    }

    /// Whether the trailing command was syntactically invalid.
    pub const fn is_trailing_invalid(&self) -> bool {
        matches!(
            self.trailing,
            BatchTail::Rejection(RequestRejection::Invalid)
        )
    }

    /// Wire length for the trailing oversized command, if any.
    pub const fn trailing_wire_len(&self) -> usize {
        match self.trailing {
            BatchTail::Rejection(RequestRejection::Oversized { wire_len }) => wire_len,
            BatchTail::None
            | BatchTail::Context(_)
            | BatchTail::Rejection(RequestRejection::Invalid) => 0,
        }
    }

    /// Whether the *first* command (blocking read) exceeded the 512-byte limit.
    /// When true, the batch is otherwise empty — caller should send 501 and continue.
    pub const fn is_first_oversized(&self) -> bool {
        matches!(
            self.contents,
            BatchContents::Rejected(RequestRejection::Oversized { .. })
        )
    }

    /// Whether the first command was syntactically invalid.
    pub const fn is_first_invalid(&self) -> bool {
        matches!(
            self.contents,
            BatchContents::Rejected(RequestRejection::Invalid)
        )
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
        let request = match line {
            CommandLine::Eof => return Ok(RequestBatch::empty()),
            CommandLine::Invalid => return Ok(RequestBatch::first_invalid()),
            CommandLine::Oversized { wire_len } => {
                return Ok(RequestBatch::first_oversized(wire_len));
            }
            CommandLine::Parsed(request) => request,
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
            match line {
                CommandLine::Eof => break,
                CommandLine::Oversized { wire_len } => {
                    return Ok(RequestBatch::contexts_with_trailing_oversized(
                        batch_contexts,
                        wire_len,
                    ));
                }
                CommandLine::Invalid => {
                    return Ok(RequestBatch::contexts_with_trailing_invalid(batch_contexts));
                }
                CommandLine::Parsed(request) => {
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

    use super::{BatchTail, MAX_PIPELINE_DEPTH, RequestBatch};
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
    #[test]
    fn pipeline_products_keep_the_inline_batch_capacity() {
        assert_eq!(MAX_PIPELINE_DEPTH, 16);
        assert!(std::mem::size_of::<RequestBatch>() > 0);
        assert!(std::mem::size_of::<BatchTail>() > 0);
    }
}
