//! Multiline and single-line backend response framing.
//!
//! This is the only module that may inspect NNTP response boundaries. Callers
//! provide the request context plus bytes read from the backend, and receive
//! typed operations such as write, capture, observe, or ordered client writes.
//! If a response is incomplete, the same framer state is fed the next backend
//! buffer until it can produce a typed complete or incomplete response chunk.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::LazyLock;

use anyhow::Context;
use smallvec::SmallVec;
use tokio::io::{AsyncWrite, AsyncWriteExt};

const TERMINATOR: &[u8; 5] = b"\r\n.\r\n";
const DOT_TERMINATOR: &[u8; 3] = b".\r\n";
const TERMINATOR_TAIL_SIZE: usize = 4;
const MAX_CAPTURED_MULTILINE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

static TERMINATOR_FINDER: LazyLock<memchr::memmem::Finder<'static>> =
    LazyLock::new(|| memchr::memmem::Finder::new(TERMINATOR));

#[must_use]
pub(crate) fn cached_response_completion() -> std::io::IoSlice<'static> {
    std::io::IoSlice::new(DOT_TERMINATOR)
}

#[cfg(test)]
pub(crate) fn empty_multiline_response_fixture(status: u16) -> Vec<u8> {
    let mut response = format!("{status} 0 <test@example.com>\r\n").into_bytes();
    response.extend(DOT_TERMINATOR.iter().copied());
    response
}

pub(crate) const CAPABILITIES_WITHOUT_AUTHINFO_RESPONSE: &[u8] =
    b"101 Capability list:\r\nVERSION 2\r\nREADER\r\nOVER\r\nHDR\r\n.\r\n";

pub(crate) const CAPABILITIES_WITH_AUTHINFO_RESPONSE: &[u8] =
    b"101 Capability list:\r\nVERSION 2\r\nREADER\r\nAUTHINFO USER PASS\r\nOVER\r\nHDR\r\n.\r\n";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ResponseWriteStats {
    chunk_count: usize,
    bytes_written: usize,
    tiny_chunks: usize,
    tiny_chunk_bytes: usize,
    small_chunks: usize,
    small_chunk_bytes: usize,
}

impl ResponseWriteStats {
    fn add_chunk(&mut self, bytes: usize) {
        let (tiny_chunks, tiny_chunk_bytes, small_chunks, small_chunk_bytes) =
            crate::pool::buffer::classify_response_write_chunk(bytes);
        self.chunk_count += 1;
        self.bytes_written += bytes;
        self.tiny_chunks += tiny_chunks;
        self.tiny_chunk_bytes += tiny_chunk_bytes;
        self.small_chunks += small_chunks;
        self.small_chunk_bytes += small_chunk_bytes;
    }

    fn add_buffered_response(&mut self, response: &crate::pool::ChunkedResponse) {
        for chunk in response.iter_chunks() {
            self.add_chunk(chunk.len());
        }
    }

    fn record(self) {
        crate::pool::buffer::record_response_write_metrics_internal(
            self.chunk_count,
            self.bytes_written,
            self.tiny_chunks,
            self.tiny_chunk_bytes,
            self.small_chunks,
            self.small_chunk_bytes,
        );
    }

    const fn bytes_written_u64(self) -> u64 {
        self.bytes_written as u64
    }
}

impl std::ops::AddAssign for ResponseWriteStats {
    fn add_assign(&mut self, rhs: Self) {
        self.chunk_count += rhs.chunk_count;
        self.bytes_written += rhs.bytes_written;
        self.tiny_chunks += rhs.tiny_chunks;
        self.tiny_chunk_bytes += rhs.tiny_chunk_bytes;
        self.small_chunks += rhs.small_chunks;
        self.small_chunk_bytes += rhs.small_chunk_bytes;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackedPendingBytesPolicy {
    Reject,
    AllowIfStatusPrefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompleteMultilinePayloadSplit {
    body: Range<usize>,
    terminator: Range<usize>,
}

impl CompleteMultilinePayloadSplit {
    #[must_use]
    const fn new(body: Range<usize>, terminator: Range<usize>) -> Self {
        Self { body, terminator }
    }

    #[must_use]
    fn body(&self) -> Range<usize> {
        self.body.clone()
    }

    #[must_use]
    #[cfg(test)]
    fn terminator(&self) -> Range<usize> {
        self.terminator.clone()
    }
}

/// Complete response window in the current backend buffer.
///
/// The ranges remain private to this module so callers cannot make their own
/// response-boundary decisions after framing.
#[derive(Debug, PartialEq, Eq)]
struct CompleteResponseWindow {
    response: Range<usize>,
    /// The response bytes intended for isolated capture.
    ///
    /// Multiline framing proves the terminator while producing this window,
    /// so capture can use the already-known payload boundary without scanning
    /// the completed bytes a second time. Single-line responses use their
    /// complete response range here.
    capture: Range<usize>,
    next_response_input: Range<usize>,
}

impl CompleteResponseWindow {
    async fn write_from<W>(
        &self,
        writer: &mut W,
        io_buffer: &mut crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        pool: &crate::pool::BufferPool,
    ) -> Result<ResponseWriteStats, crate::session::response_transfer::ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        write_response_chunk_preserving_suffix_on_error(writer, io_buffer, conn, pool, self).await
    }

    fn extend_capture_from(&self, source: &[u8], capture: &mut crate::pool::PooledBuffer) {
        capture.extend_from_slice(&source[self.response.clone()]);
    }

    fn extend_payload_capture_from(&self, source: &[u8], capture: &mut crate::pool::PooledBuffer) {
        capture.extend_from_slice(&source[self.capture.clone()]);
    }

    fn push_isolated_buffer_to(
        &self,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
        buffer: &mut crate::pool::PooledBuffer,
    ) {
        let old = std::mem::replace(buffer, pool.acquire());
        response.push_buffer_range(old, self.response.clone());
    }

    fn queue_next_response_input(
        &self,
        buffer: &[u8],
        conn: &mut crate::stream::ConnectionStream,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        if self.next_response_input.start < buffer.len() {
            conn.queue_pending_bytes_first(&buffer[self.next_response_input.clone()])
                .map_err(crate::session::response_transfer::ResponseTransferError::Io)?;
        }
        Ok(())
    }

    fn queue_pooled_next_response_input(
        &self,
        io_buffer: &mut crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        pool: &crate::pool::BufferPool,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        let total_len = io_buffer.initialized();
        if self.next_response_input.start < total_len {
            let old = std::mem::replace(io_buffer, pool.acquire());
            conn.queue_pooled_pending_bytes_first(old, self.next_response_input.clone())
                .map_err(crate::session::response_transfer::ResponseTransferError::Io)?;
        }
        Ok(())
    }

    fn push_from_buffer(
        &self,
        io_buffer: &mut crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        let total_len = io_buffer.initialized();
        let old = std::mem::replace(io_buffer, pool.acquire());
        if self.next_response_input.start < total_len {
            conn.queue_pending_bytes_first(&old[self.next_response_input.clone()])
                .map_err(crate::session::response_transfer::ResponseTransferError::Io)?;
        }

        response.push_buffer_range(old, self.response.clone());
        Ok(())
    }

    fn observe_from_buffer(
        &self,
        io_buffer: &mut crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        pool: &crate::pool::BufferPool,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        let total_len = io_buffer.initialized();
        if self.next_response_input.start < total_len {
            let old = std::mem::replace(io_buffer, pool.acquire());
            conn.queue_pooled_pending_bytes_first(old, self.next_response_input.clone())
                .map_err(crate::session::response_transfer::ResponseTransferError::Io)?;
        }
        Ok(())
    }
}

/// Reuse the framer-owned packed suffix, or acquire an empty read buffer.
pub(crate) fn take_queued_input_or_acquire_empty(
    conn: &mut crate::stream::ConnectionStream,
    pool: &crate::pool::BufferPool,
) -> crate::pool::PooledBuffer {
    let Some((mut buffer, range)) = conn.take_leading_pooled_pending_input() else {
        return pool.acquire();
    };
    buffer.expose_initialized_range_without_copying(range);
    buffer
}

/// Receive the next response after its request was already written as part of
/// a pipeline window, retaining the connection in the same exchange as the
/// queued input and request-scoped framer.
pub(crate) async fn read_exchange_for_already_sent_request<'pool>(
    mut conn: crate::pool::ConnectionGuard,
    request: &crate::protocol::RequestContext,
    pool: &'pool crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> anyhow::Result<BackendResponseExchange<'pool>> {
    let mut buffer = take_queued_input_or_acquire_empty(conn.stream_mut(), pool);
    if buffer.initialized() == 0 {
        let n = buffer.read_from(conn.stream_mut()).await?;
        if n == 0 {
            anyhow::bail!("Backend connection closed unexpectedly");
        }
    }

    let response = ClassifiedResponse::read_unbound(conn.stream_mut(), request, buffer).await?;
    Ok(BackendResponseExchange::new(
        conn, response, pool, backend_id,
    ))
}

async fn write_response_chunk_preserving_suffix_on_error<W>(
    writer: &mut W,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    pool: &crate::pool::BufferPool,
    window: &CompleteResponseWindow,
) -> Result<ResponseWriteStats, crate::session::response_transfer::ResponseTransferError>
where
    W: AsyncWrite + Unpin,
{
    let total_len = io_buffer.initialized();
    let chunk = &io_buffer[..total_len][window.response.clone()];
    if let Err(err) = writer.write_all(chunk).await {
        window.queue_pooled_next_response_input(io_buffer, conn, pool)?;
        return Err(
            crate::session::response_transfer::ResponseTransferError::ClientDisconnect(err),
        );
    }
    let mut stats = ResponseWriteStats::default();
    stats.add_chunk(chunk.len());
    window.queue_pooled_next_response_input(io_buffer, conn, pool)?;
    Ok(stats)
}

#[derive(Debug, PartialEq, Eq)]
struct IncompleteResponseWindow {
    response: Range<usize>,
}

impl IncompleteResponseWindow {
    fn extend_capture_from(&self, source: &[u8], capture: &mut crate::pool::PooledBuffer) {
        capture.extend_from_slice(&source[self.response.clone()]);
    }

    fn push_buffer_to(
        &self,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
        buffer: &mut crate::pool::PooledBuffer,
    ) {
        let old = std::mem::replace(buffer, pool.acquire());
        response.push_buffer_range(old, self.response.clone());
    }
}

/// Count consumed from one scanner push, never from an accumulated window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkConsumed(usize);

/// Exclusive payload end from one scanner push, excluding the wire
/// terminator. It is distinct from the response end consumed by the framer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkPayloadEnd(usize);

/// Exclusive origin of the logical response window supplied to one scanner
/// push. It is not itself proof that a response is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowOffset(usize);

impl WindowOffset {
    fn new(value: usize) -> Self {
        Self(value)
    }

    fn get(self) -> usize {
        self.0
    }

    fn after_chunk(self, consumed: ChunkConsumed) -> crate::protocol::FrameEnd {
        crate::protocol::FrameEnd::new(self.0 + consumed.0)
    }

    fn after_payload_chunk(self, payload_end: ChunkPayloadEnd) -> crate::protocol::FrameEnd {
        crate::protocol::FrameEnd::new(self.0 + payload_end.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompleteChunk {
    consumed: ChunkConsumed,
    payload_end: ChunkPayloadEnd,
}

#[derive(Debug, PartialEq, Eq)]
enum ChunkProgress {
    Complete(CompleteChunk),
    Incomplete,
}

impl ChunkProgress {
    // Translation lives here; operation contexts supply the origin from their
    // own buffer-bound append result, never from an application caller.
    fn in_window(self, origin: WindowOffset, window_len: usize) -> ResponseWindow {
        match self {
            Self::Complete(complete) => {
                let end = origin.after_chunk(complete.consumed);
                ResponseWindow::Complete(CompleteResponseWindow {
                    response: 0..end.get(),
                    capture: 0..origin.after_payload_chunk(complete.payload_end).get(),
                    next_response_input: end.get()..window_len,
                })
            }
            Self::Incomplete => ResponseWindow::Incomplete(IncompleteResponseWindow {
                response: 0..window_len,
            }),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ResponseWindow {
    Complete(CompleteResponseWindow),
    Incomplete(IncompleteResponseWindow),
}

/// The protocol cursor that owns one response's mutable framing context.
///
/// Keeping the connection, visible buffer, scanner continuation, current
/// window, and packed-response policy together prevents an operation from
/// pairing a continuation with a different buffer or backend stream. The
/// higher-level isolated and streaming operations differ in what they do with
/// a completed window, not in who owns the framing state.
struct ResponseCursor<'a> {
    conn: &'a mut crate::stream::ConnectionStream,
    io_buffer: &'a mut crate::pool::PooledBuffer,
    framer: MultilineFramer,
    frame: ResponseWindow,
    packed_policy: PackedPendingBytesPolicy,
}

impl<'a> ResponseCursor<'a> {
    fn begin(
        conn: &'a mut crate::stream::ConnectionStream,
        io_buffer: &'a mut crate::pool::PooledBuffer,
        packed_policy: PackedPendingBytesPolicy,
    ) -> Result<Self, FramingError> {
        let mut framer = MultilineFramer::default();
        let frame = framer.frame_with_policy(io_buffer, packed_policy)?;
        Ok(Self {
            conn,
            io_buffer,
            framer,
            frame,
            packed_policy,
        })
    }

    /// Start a multiline cursor after the request-scoped status line has
    /// already been classified.  The status-line bytes still seed the
    /// rolling tail because an empty body forms `\r\n.\r\n` across that
    /// boundary, but they do not need to be searched a second time.
    fn begin_after_status_line(
        conn: &'a mut crate::stream::ConnectionStream,
        io_buffer: &'a mut crate::pool::PooledBuffer,
        status_line_end: crate::protocol::StatusLineEnd,
        packed_policy: PackedPendingBytesPolicy,
    ) -> Result<Self, FramingError> {
        let total_len = io_buffer.initialized();
        let status_line_end = status_line_end.get();
        if status_line_end > total_len {
            return Err(FramingError::InvalidFrameMetadata);
        }

        let mut framer = MultilineFramer::default();
        framer.update(&io_buffer[..status_line_end]);
        let frame = framer
            .split_chunk(&io_buffer[status_line_end..total_len], packed_policy)
            .map(|progress| progress.in_window(WindowOffset::new(status_line_end), total_len))?;
        Ok(Self {
            conn,
            io_buffer,
            framer,
            frame,
            packed_policy,
        })
    }

    fn frame_next_chunk(&mut self) -> Result<(), FramingError> {
        self.frame = self
            .framer
            .frame_with_policy(self.io_buffer, self.packed_policy)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FramedSingleLineChunk {
    response: Range<usize>,
    next_response_input: Range<usize>,
}

impl FramedSingleLineChunk {
    fn require_isolated(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.next_response_input.is_empty(),
            "unexpected bytes after isolated single-line response"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FramedResponseForRequest {
    response: BackendChunkRange,
}

impl FramedResponseForRequest {
    #[must_use]
    fn backend_bytes<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        self.response.slice(source)
    }

    #[must_use]
    fn end(&self) -> BackendChunkEnd {
        self.response.end()
    }
}

/// Exclusive end position within the backend read currently being tracked.
///
/// This coordinate is relative to one `accept_backend_bytes` input slice. It
/// is not a response-window coordinate and cannot be passed to the multiline
/// framer without an explicit conversion inside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackendChunkEnd(usize);

impl BackendChunkEnd {
    #[must_use]
    const fn new(value: usize) -> Self {
        Self(value)
    }

    #[must_use]
    const fn get(self) -> usize {
        self.0
    }

    #[must_use]
    fn after(self, consumed: ChunkConsumed) -> Self {
        Self(self.0 + consumed.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackendChunkRange {
    start: BackendChunkEnd,
    end: BackendChunkEnd,
}

impl BackendChunkRange {
    #[must_use]
    const fn new(start: BackendChunkEnd, end: BackendChunkEnd) -> Self {
        Self { start, end }
    }

    #[must_use]
    fn slice(self, source: &[u8]) -> &[u8] {
        &source[self.start.get()..self.end.get()]
    }

    #[must_use]
    const fn end(self) -> BackendChunkEnd {
        self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendReplyBytes<'a> {
    CompletedTrackedReply(&'a [u8]),
    ForwardUntracked(&'a [u8]),
}

#[derive(Debug)]
enum OrderedResponse {
    Backend,
    Local(&'static [u8]),
}

pub(crate) type OrderedClientWrites<'a> = SmallVec<[Cow<'a, [u8]>; 4]>;
pub(crate) type ReadyDeferredReplies = SmallVec<[&'static [u8]; 2]>;

/// Request-aware ordering layer for pipelined backend bytes and local replies.
///
/// It wraps the private reply tracker so stateful session code can enqueue
/// expected backend replies and local deferred replies without seeing response
/// boundary details.
#[derive(Default, Debug)]
pub(crate) struct BackendResponseOrder {
    inner: BackendReplyTracker,
    ordered: VecDeque<OrderedResponse>,
}

impl BackendResponseOrder {
    /// Register that the next backend bytes should contain a reply for `kind`.
    pub(crate) fn push_request(&mut self, kind: crate::protocol::RequestKind) {
        self.inner.push_request(kind);
        self.ordered.push_back(OrderedResponse::Backend);
    }

    /// Whether at least one registered backend reply still needs backend bytes.
    #[inline]
    pub(crate) fn has_pending_backend_replies(&self) -> bool {
        self.ordered
            .iter()
            .any(|reply| matches!(reply, OrderedResponse::Backend))
    }

    /// Queue a local reply that must be written only after earlier backend
    /// replies have been framed and forwarded.
    pub(crate) fn push_deferred_reply(&mut self, reply: &'static [u8]) {
        self.ordered.push_back(OrderedResponse::Local(reply));
    }

    /// Whether local replies are waiting behind backend replies.
    #[inline]
    pub(crate) fn has_deferred_replies(&self) -> bool {
        self.ordered
            .iter()
            .any(|reply| matches!(reply, OrderedResponse::Local(_)))
    }

    /// Whether a local reply is blocked behind an earlier backend reply.
    pub(crate) fn should_drain_backend_replies(&self) -> bool {
        let has_deferred_reply_behind_front = self
            .ordered
            .iter()
            .skip(1)
            .any(|reply| matches!(reply, OrderedResponse::Local(_)));

        matches!(self.ordered.front(), Some(OrderedResponse::Backend))
            && has_deferred_reply_behind_front
    }

    /// Remove local replies that are ready before the next backend reply.
    pub(crate) fn take_ready_deferred_replies(&mut self) -> ReadyDeferredReplies {
        let mut replies = SmallVec::new();
        while let Some(OrderedResponse::Local(_)) = self.ordered.front() {
            let Some(OrderedResponse::Local(reply)) = self.ordered.pop_front() else {
                break;
            };
            replies.push(reply);
        }
        replies
    }

    /// Convert raw backend bytes into ordered client writes.
    ///
    /// Returned borrowed slices are already complete responses for their
    /// registered requests; any local replies unblocked by those responses are
    /// returned as owned buffers in the same order.
    pub(crate) fn client_writes_for_backend_read<'a>(
        &mut self,
        backend_read: &'a [u8],
    ) -> OrderedClientWrites<'a> {
        let mut writes = SmallVec::new();

        for reply in self.take_ready_deferred_replies() {
            writes.push(Cow::Borrowed(reply));
        }

        for reply in self.inner.accept_backend_bytes(backend_read) {
            match reply {
                BackendReplyBytes::CompletedTrackedReply(bytes) => {
                    writes.push(Cow::Borrowed(bytes));
                    if matches!(self.ordered.front(), Some(OrderedResponse::Backend)) {
                        self.ordered.pop_front();
                    }
                    for reply in self.take_ready_deferred_replies() {
                        writes.push(Cow::Borrowed(reply));
                    }
                }
                BackendReplyBytes::ForwardUntracked(bytes) => {
                    writes.push(Cow::Borrowed(bytes));
                }
            }
        }

        writes
    }
}

#[derive(Default, Debug)]
struct BackendReplyTracker {
    pending: VecDeque<PendingRequestFrame>,
}

#[derive(Debug)]
struct PendingRequestFrame {
    kind: crate::protocol::RequestKind,
    state: PendingRequestFrameState,
    status_line: smallvec::SmallVec<[u8; crate::constants::buffer::COMMAND]>,
}

#[derive(Debug)]
enum PendingRequestFrameState {
    AwaitingStatusLine,
    ReadingMultiline { framer: MultilineFramer },
}

#[derive(Default, Debug)]
struct MultilineFramer {
    data: [u8; TERMINATOR_TAIL_SIZE],
    len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FramingError {
    UnexpectedTrailingResponseBytes,
    InvalidFrameMetadata,
    BackendEof,
    Io,
    CapturedResponseTooLarge { bytes: usize, max: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResponseWarning {
    ShortResponse { bytes: usize, min: usize },
    InvalidResponse,
    UnusualStatusCode(u16),
}

#[derive(Debug, PartialEq, Eq)]
struct ResponseStatusParse {
    status_code: Option<crate::protocol::StatusCode>,
    warnings: smallvec::SmallVec<[ResponseWarning; 0]>,
}

#[must_use]
fn parse_response_status(chunk: &[u8]) -> ResponseStatusParse {
    let mut warnings = smallvec::SmallVec::new();

    if chunk.len() < crate::protocol::MIN_RESPONSE_LENGTH {
        warnings.push(ResponseWarning::ShortResponse {
            bytes: chunk.len(),
            min: crate::protocol::MIN_RESPONSE_LENGTH,
        });
    }

    let status_code = crate::protocol::StatusCode::parse(chunk);
    if let Some(code) = status_code {
        let raw_code = code.as_u16();
        if raw_code == 0 || raw_code >= 600 {
            warnings.push(ResponseWarning::UnusualStatusCode(raw_code));
        }
    } else {
        warnings.push(ResponseWarning::InvalidResponse);
    }

    ResponseStatusParse {
        status_code,
        warnings,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResponseFrame {
    SingleLine {
        status: crate::protocol::StatusCode,
        status_line_end: crate::protocol::StatusLineEnd,
        framed: FramedSingleLineChunk,
    },
    Multiline {
        status: crate::protocol::StatusCode,
        status_line_end: crate::protocol::StatusLineEnd,
    },
}

impl ResponseFrame {
    fn parse(
        request: &crate::protocol::RequestContext,
        buffer: &crate::pool::PooledBuffer,
    ) -> Result<Self, ResponseReadError> {
        Self::parse_bytes(request, &buffer[..buffer.initialized()])
    }

    fn parse_bytes(
        request: &crate::protocol::RequestContext,
        chunk: &[u8],
    ) -> Result<Self, ResponseReadError> {
        let Some(status_line_end) = status_line_len(chunk) else {
            return Err(ResponseReadError::Incomplete);
        };
        let parsed = parse_response_status(chunk);
        let Some(status) = parsed.status_code else {
            return Err(ResponseReadError::Invalid(parsed.warnings));
        };
        if request.has_response_body(status) {
            return Ok(Self::Multiline {
                status,
                status_line_end: crate::protocol::StatusLineEnd::new(status_line_end),
            });
        }

        Ok(Self::SingleLine {
            status,
            status_line_end: crate::protocol::StatusLineEnd::new(status_line_end),
            framed: FramedSingleLineChunk {
                response: 0..status_line_end,
                next_response_input: status_line_end..chunk.len(),
            },
        })
    }
}

/// Classification retains its input and request-scoped shape. The buffer cannot
/// be replaced or mutated before the consuming transfer operation uses it.
pub(crate) struct ClassifiedResponse {
    buffer: crate::pool::PooledBuffer,
    frame: Result<ResponseFrame, ResponseReadError>,
    kind: crate::protocol::RequestKind,
}

/// Owns a classified response and the backend connection that supplied it.
///
/// The response buffer and connection are deliberately moved together.  A
/// caller can inspect the classification, then consume the exchange through
/// `receiving()`; it cannot pair the classified bytes with another connection
/// or forget which pool/window owns the continuation.
pub(crate) struct BackendResponseExchange<'pool> {
    conn: crate::pool::ConnectionGuard,
    response: Option<ClassifiedResponse>,
    pool: &'pool crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
}

/// An invalid response together with the backend connection that supplied it.
///
/// Invalid bytes are retained only for diagnostics and health-check salvage;
/// callers cannot detach them from the connection and accidentally apply a
/// failure policy to a different exchange.
pub(crate) struct InvalidBackendResponse {
    buffer: crate::pool::PooledBuffer,
    conn: crate::pool::ConnectionGuard,
}

impl InvalidBackendResponse {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buffer
    }

    pub(crate) fn into_connection_for_health_check(
        self,
    ) -> crate::pool::deadpool_connection::PooledConnection {
        self.conn.into_connection_for_health_check()
    }
}

impl<'pool> BackendResponseExchange<'pool> {
    /// Read one response and bind it to the connection, pool, and backend
    /// identity that supplied its bytes.
    pub(crate) async fn read(
        mut conn: crate::pool::ConnectionGuard,
        request: &crate::protocol::RequestContext,
        buffer: crate::pool::PooledBuffer,
        pool: &'pool crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> anyhow::Result<Self> {
        let response = ClassifiedResponse::read_unbound(conn.stream_mut(), request, buffer).await?;
        Ok(Self::new(conn, response, pool, backend_id))
    }

    fn new(
        conn: crate::pool::ConnectionGuard,
        response: ClassifiedResponse,
        pool: &'pool crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> Self {
        Self {
            conn,
            response: Some(response),
            pool,
            backend_id,
        }
    }

    /// Construct an exchange for tests that inject an already-read response.
    /// Production callers must use `read` so the response and guard are bound
    /// by the same read operation.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        conn: crate::pool::ConnectionGuard,
        response: ClassifiedResponse,
        pool: &'pool crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> Self {
        Self::new(conn, response, pool, backend_id)
    }

    pub(crate) fn status_code(&self) -> Option<crate::protocol::StatusCode> {
        self.response
            .as_ref()
            .and_then(ClassifiedResponse::status_code)
    }

    pub(crate) const fn backend_id(&self) -> crate::types::BackendId {
        self.backend_id
    }

    pub(crate) fn received_len(&self) -> usize {
        self.response
            .as_ref()
            .map_or(0, ClassifiedResponse::received_len)
    }

    /// Borrow a complete single-line payload before selecting its consuming
    /// operation. The bytes remain owned by this exchange and cannot be paired
    /// with another connection or response state.
    pub(crate) fn single_line_bytes(&self) -> Option<&[u8]> {
        self.response
            .as_ref()
            .and_then(ClassifiedResponse::single_line_bytes)
    }

    pub(crate) fn log_warnings(&self, client_addr: impl std::fmt::Display) {
        if let Some(response) = &self.response {
            response.log_warnings(client_addr, self.backend_id);
        }
    }

    /// Start the sole consuming operation for this exchange.
    ///
    /// Taking the response out makes the transition one-shot.  The returned
    /// operation borrows this exchange's connection, so the connection cannot
    /// be released or replaced while framing/forwarding is in progress.
    pub(crate) fn receiving(&mut self) -> anyhow::Result<ReceivingResponse<'_>> {
        let response = self
            .response
            .take()
            .ok_or_else(|| anyhow::anyhow!("backend response operation already consumed"))?;
        Ok(response.receiving(&mut self.conn, self.pool, self.backend_id))
    }

    /// Move out the backend connection after the response operation was taken.
    ///
    /// A successful connection handoff without first consuming the classified
    /// response would leave unread protocol bytes behind. Keep that invalid
    /// transition explicit at this boundary instead of relying on every caller
    /// to remember the ordering rule.
    #[inline]
    pub(crate) fn into_connection(self) -> crate::pool::ConnectionGuard {
        debug_assert!(
            self.response.is_none(),
            "cannot release a backend exchange before consuming its response"
        );
        assert!(
            self.conn.response_is_complete(),
            "cannot release a backend exchange before completing its response"
        );
        self.conn
    }

    /// Move out the backend connection after a failed or abandoned response
    /// operation so the caller can retire it. Successful reuse must use
    /// [`Self::into_connection`], which requires the framer's completion mark.
    pub(crate) fn into_connection_after_failure(self) -> crate::pool::ConnectionGuard {
        debug_assert!(
            self.response.is_none(),
            "cannot handle a failed backend exchange before consuming its response"
        );
        self.conn
    }

    /// Move an invalid response and its connection together for diagnostics
    /// and health-check salvage.
    pub(crate) fn into_invalid_response(self) -> InvalidBackendResponse {
        let response = self
            .response
            .expect("invalid response conversion requires an unconsumed response");
        debug_assert!(response.frame.is_err());
        InvalidBackendResponse {
            buffer: response.buffer,
            conn: self.conn,
        }
    }

    /// Retire an exchange whose response was never consumed.
    pub(crate) fn fail_backend(self) {
        self.conn.fail_backend();
    }

    /// Capture the framed response and return its connection only after the
    /// same exchange has proved that no response bytes remain queued.
    ///
    /// Consuming the exchange keeps response ownership, framing completion,
    /// and connection reuse in one operation. Any capture or framing failure
    /// drops the owning guard instead of allowing a caller to release an
    /// unfinished connection.
    pub(crate) async fn capture_isolated_and_reuse(mut self) -> anyhow::Result<CapturedResponse> {
        let captured = self.receiving()?.capture_isolated().await?;
        let connection = self.into_connection();
        let _reusable = connection.complete_success();
        Ok(captured)
    }

    /// Drain the framed response and return its completed connection to the
    /// caller's routing policy. The exchange owns failure cleanup; only a
    /// response proven complete can produce the returned guard.
    pub(crate) async fn observe_and_reuse(
        mut self,
    ) -> anyhow::Result<crate::pool::ConnectionGuard> {
        self.receiving()?.observe().await?;
        Ok(self.into_connection())
    }
}

impl ClassifiedResponse {
    async fn read_unbound<C: tokio::io::AsyncRead + Unpin>(
        conn: &mut C,
        request: &crate::protocol::RequestContext,
        mut buffer: crate::pool::PooledBuffer,
    ) -> anyhow::Result<Self> {
        loop {
            let frame = ResponseFrame::parse(request, &buffer);
            match frame {
                Err(ResponseReadError::Incomplete) => {
                    let more = buffer.read_more(conn).await?;
                    if more == 0 {
                        if buffer.available_read_capacity() == 0 {
                            anyhow::bail!(
                                "Backend response exceeded the read buffer capacity ({} bytes)",
                                buffer.initialized()
                            );
                        }
                        anyhow::bail!(
                            "Backend EOF before complete backend response ({} bytes)",
                            buffer.initialized()
                        );
                    }
                }
                frame => {
                    return Ok(Self {
                        buffer,
                        frame,
                        kind: request.kind(),
                    });
                }
            }
        }
    }

    #[cfg(any(test, response_contract))]
    pub(crate) async fn read_for_test<C: tokio::io::AsyncRead + Unpin>(
        conn: &mut C,
        request: &crate::protocol::RequestContext,
        buffer: crate::pool::PooledBuffer,
    ) -> anyhow::Result<Self> {
        Self::read_unbound(conn, request, buffer).await
    }

    /// Bind the classified response to the same backend exchange that supplied
    /// its bytes. The resulting operation owns the only public path to
    /// forwarding, observation, or intentional capture.
    fn receiving<'a>(
        self,
        conn: &'a mut crate::pool::ConnectionGuard,
        pool: &'a crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> ReceivingResponse<'a> {
        crate::protocol::ArticleState::new(Receiving {
            response: self,
            conn,
            pool,
            backend_id,
        })
    }

    pub(crate) fn status_code(&self) -> Option<crate::protocol::StatusCode> {
        match &self.frame {
            Ok(
                ResponseFrame::SingleLine { status, .. } | ResponseFrame::Multiline { status, .. },
            ) => Some(*status),
            Err(_) => None,
        }
    }

    pub(crate) fn single_line_bytes(&self) -> Option<&[u8]> {
        match &self.frame {
            Ok(ResponseFrame::SingleLine { framed, .. }) => {
                Some(&self.buffer[framed.response.clone()])
            }
            Ok(ResponseFrame::Multiline { .. }) | Err(_) => None,
        }
    }

    pub(crate) fn received_len(&self) -> usize {
        self.buffer.len()
    }

    async fn capture_isolated(
        self,
        conn: &mut crate::pool::ConnectionGuard,
        pool: &crate::pool::BufferPool,
    ) -> anyhow::Result<CapturedResponse> {
        let ClassifiedResponse {
            buffer,
            frame,
            kind,
        } = self;
        match frame {
            Ok(ResponseFrame::SingleLine {
                status,
                status_line_end,
                framed,
            }) => {
                framed.require_isolated()?;
                conn.mark_response_complete();
                let content_end = crate::protocol::ContentEnd::new(framed.response.end);
                Ok(crate::protocol::ArticleState::new(
                    crate::protocol::FramedArticleState::new(
                        buffer,
                        kind,
                        status,
                        status_line_end,
                        content_end,
                    ),
                ))
            }
            Ok(ResponseFrame::Multiline {
                status,
                status_line_end,
            }) => {
                let mut buffer = buffer;
                let mut capture = pool.acquire_capture();
                IsolatedMultilineResponse::begin(conn.stream_mut(), &mut buffer)
                    .map_err(isolated_multiline_error)?
                    .capture_payload_into(&mut capture)
                    .await?;
                conn.mark_response_complete();
                let content_end = crate::protocol::ContentEnd::new(capture.initialized());
                Ok(crate::protocol::ArticleState::new(
                    crate::protocol::FramedArticleState::new(
                        capture,
                        kind,
                        status,
                        status_line_end,
                        content_end,
                    ),
                ))
            }
            Err(error) => anyhow::bail!("cannot capture invalid backend response: {error:?}"),
        }
    }

    async fn capture_isolated_chunked_optional(
        mut self,
        conn: &mut crate::pool::ConnectionGuard,
        pool: &crate::pool::BufferPool,
        captured: &mut crate::pool::ChunkedResponse,
    ) -> anyhow::Result<Option<CapturedChunkedResponse>> {
        let (kind, status, status_line_end) = match self.frame {
            Ok(ResponseFrame::Multiline {
                status,
                status_line_end,
            }) => (self.kind, status, status_line_end),
            _ => anyhow::bail!("isolated multiline capture requires a multiline response"),
        };
        let retained = capture_isolated_multiline_response_chunked_optional(
            conn.stream_mut(),
            &mut self.buffer,
            pool,
            captured,
        )
        .await
        .map_err(|error| anyhow::anyhow!("backend multiline response capture failed: {error:?}"))?;
        conn.mark_response_complete();
        Ok(retained.map(|payload_end| {
            CapturedChunkedResponse::from_capture(
                kind,
                status,
                status_line_end,
                std::mem::take(captured),
                payload_end,
            )
        }))
    }

    async fn observe_isolated(
        mut self,
        conn: &mut crate::pool::ConnectionGuard,
    ) -> anyhow::Result<()> {
        match self.frame {
            Ok(ResponseFrame::SingleLine { framed, .. }) => framed.require_isolated()?,
            Ok(ResponseFrame::Multiline { .. }) => {
                observe_isolated_multiline_response(conn.stream_mut(), &mut self.buffer)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("backend multiline response drain failed: {error:?}")
                    })?
            }
            Err(error) => anyhow::bail!("cannot observe invalid backend response: {error:?}"),
        }
        conn.mark_response_complete();
        Ok(())
    }

    pub(crate) fn log_warnings(
        &self,
        client_addr: impl std::fmt::Display,
        backend_id: crate::types::BackendId,
    ) {
        if let Err(error) = &self.frame {
            error.log_warnings(&self.buffer, client_addr, backend_id);
        }
    }

    pub(crate) fn require_isolated_single_line(&self) -> anyhow::Result<()> {
        match &self.frame {
            Ok(ResponseFrame::SingleLine { framed, .. }) => {
                framed.require_isolated()?;
                Ok(())
            }
            Ok(ResponseFrame::Multiline { .. }) => {
                anyhow::bail!("multiline response requires framer-owned completion")
            }
            Err(_) => anyhow::bail!("cannot complete an invalid backend response"),
        }
    }

    /// Complete a successfully isolated single-line response on its owning
    /// connection. Keeping the isolation check and guard transition together
    /// prevents callers from pairing an independent proof with a connection.
    fn complete_single_line(self, conn: &mut crate::pool::ConnectionGuard) -> anyhow::Result<()> {
        self.require_isolated_single_line()?;
        conn.mark_response_complete();
        Ok(())
    }

    #[cfg(any(test, response_contract))]
    fn stream_for_test<'a>(
        &'a mut self,
        conn: &'a mut crate::stream::ConnectionStream,
        pool: &'a crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> Result<StreamingResponse<'a>, crate::session::response_transfer::ResponseTransferError>
    {
        let frame = std::mem::replace(&mut self.frame, Err(ResponseReadError::Incomplete));
        StreamingResponse::from_classification(
            self.kind,
            frame,
            &mut self.buffer,
            conn,
            pool,
            backend_id,
        )
    }
}

/// A classified response and its owning backend exchange.
///
/// The borrow ties the response's continuation, pooled bytes, and connection
/// together until one consuming operation completes or retires the exchange.
pub(crate) struct Receiving<'a> {
    response: ClassifiedResponse,
    conn: &'a mut crate::pool::ConnectionGuard,
    pool: &'a crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
}

/// An active response operation bound to the connection that supplied it.
pub(crate) type ReceivingResponse<'a> = crate::protocol::ArticleState<Receiving<'a>>;

/// An intentionally captured response that retains the bytes produced by the
/// framer. The status, request shape, and owner travel together.
type CapturedResponse =
    crate::protocol::ArticleState<crate::protocol::FramedArticleState<crate::pool::PooledBuffer>>;

/// A retained response whose cache payload boundary was established by the
/// framer. The cache adapter receives this resource-bound state instead of
/// re-scanning the captured bytes for the multiline terminator.
pub(crate) type CapturedChunkedResponse = crate::protocol::ArticleState<
    crate::protocol::FramedArticleState<crate::pool::ChunkedResponse>,
>;

impl CapturedChunkedResponse {
    pub(crate) fn from_capture(
        kind: crate::protocol::RequestKind,
        status: crate::protocol::StatusCode,
        status_line_end: crate::protocol::StatusLineEnd,
        bytes: crate::pool::ChunkedResponse,
        payload_end: crate::cache::CachePayloadEnd,
    ) -> Self {
        let content_end = crate::protocol::ContentEnd::new(payload_end.as_usize());
        debug_assert!(payload_end.as_usize() <= bytes.len());
        let state = crate::protocol::ArticleState::new(crate::protocol::FramedArticleState::new(
            bytes,
            kind,
            status,
            status_line_end,
            content_end,
        ));
        debug_assert_eq!(state.as_inner().content_end().get(), payload_end.as_usize());
        state
    }

    pub(crate) fn into_cache_ingest(self) -> crate::cache::FramedChunkedResponse {
        crate::cache::FramedChunkedResponse::from_article_state(self)
    }

    pub(crate) fn len(&self) -> usize {
        self.as_inner().bytes().len()
    }
}

impl Receiving<'_> {
    fn stream(
        &mut self,
    ) -> Result<StreamingResponse<'_>, crate::session::response_transfer::ResponseTransferError>
    {
        let Self {
            response,
            conn,
            pool,
            backend_id,
        } = self;
        let frame = std::mem::replace(&mut response.frame, Err(ResponseReadError::Incomplete));
        StreamingResponse::from_classification(
            response.kind,
            frame,
            &mut response.buffer,
            conn.stream_mut(),
            pool,
            *backend_id,
        )
    }

    #[must_use]
    pub(crate) fn status_code(&self) -> Option<crate::protocol::StatusCode> {
        self.response.status_code()
    }

    pub(crate) async fn capture_isolated(self) -> anyhow::Result<CapturedResponse> {
        let Self {
            response,
            conn,
            pool,
            ..
        } = self;
        response.capture_isolated(conn, pool).await
    }

    pub(crate) async fn capture_isolated_chunked_optional(
        self,
        captured: &mut crate::pool::ChunkedResponse,
    ) -> anyhow::Result<Option<CapturedChunkedResponse>> {
        let Self {
            response,
            conn,
            pool,
            ..
        } = self;
        response
            .capture_isolated_chunked_optional(conn, pool, captured)
            .await
    }

    pub(crate) async fn observe_isolated(self) -> anyhow::Result<()> {
        let Self { response, conn, .. } = self;
        response.observe_isolated(conn).await
    }

    pub(crate) fn complete_single_line(self) -> anyhow::Result<()> {
        let Self { response, conn, .. } = self;
        response.complete_single_line(conn)
    }

    pub(crate) async fn write<W: AsyncWrite + Unpin>(
        self,
        writer: &mut W,
    ) -> Result<u64, crate::session::response_transfer::ResponseTransferError> {
        let mut receiving = self;
        let (result, completed) = {
            let mut streaming = receiving.stream()?;
            let result = streaming.write(writer).await;
            let completed = streaming.is_complete();
            (result, completed)
        };
        if completed {
            receiving.conn.mark_response_complete();
        }
        let stats = result?;
        stats.record();
        Ok(stats.bytes_written_u64())
    }

    pub(crate) async fn observe(
        self,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        let mut receiving = self;
        receiving.stream()?.observe().await?;
        receiving.conn.mark_response_complete();
        Ok(())
    }

    pub(crate) async fn capture_and_write<W: AsyncWrite + Unpin>(
        self,
        writer: &mut W,
        captured: &mut crate::pool::ChunkedResponse,
    ) -> Result<
        (u64, Option<CapturedChunkedResponse>),
        crate::session::response_transfer::ResponseTransferError,
    > {
        let mut receiving = self;
        let (result, completed) = {
            let mut streaming = receiving.stream()?;
            let result = streaming
                .capture_and_write(writer, captured, MAX_CAPTURED_MULTILINE_RESPONSE_BYTES)
                .await;
            let completed = streaming.is_complete();
            (result, completed)
        };
        if completed {
            receiving.conn.mark_response_complete();
        }
        let result = result?;
        Ok(result)
    }
}

impl crate::protocol::ArticleState<Receiving<'_>> {
    #[must_use]
    pub(crate) fn status_code(&self) -> Option<crate::protocol::StatusCode> {
        self.as_inner().status_code()
    }

    pub(crate) async fn capture_isolated(self) -> anyhow::Result<CapturedResponse> {
        self.into_inner().capture_isolated().await
    }

    pub(crate) async fn capture_isolated_chunked_optional(
        self,
        captured: &mut crate::pool::ChunkedResponse,
    ) -> anyhow::Result<Option<CapturedChunkedResponse>> {
        self.into_inner()
            .capture_isolated_chunked_optional(captured)
            .await
    }

    pub(crate) async fn observe_isolated(self) -> anyhow::Result<()> {
        self.into_inner().observe_isolated().await
    }

    pub(crate) fn complete_single_line(self) -> anyhow::Result<()> {
        self.into_inner().complete_single_line()
    }

    pub(crate) async fn write<W: AsyncWrite + Unpin>(
        self,
        writer: &mut W,
    ) -> Result<u64, crate::session::response_transfer::ResponseTransferError> {
        self.into_inner().write(writer).await
    }

    pub(crate) async fn observe(
        self,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        self.into_inner().observe().await
    }

    pub(crate) async fn capture_and_write<W: AsyncWrite + Unpin>(
        self,
        writer: &mut W,
        captured: &mut crate::pool::ChunkedResponse,
    ) -> Result<
        (u64, Option<CapturedChunkedResponse>),
        crate::session::response_transfer::ResponseTransferError,
    > {
        self.into_inner().capture_and_write(writer, captured).await
    }
}

pub(crate) fn unpacked_single_line_response<'a>(
    request: &crate::protocol::RequestContext,
    bytes: &'a [u8],
) -> Result<&'a [u8], ResponseReadError> {
    ResponseFrame::parse_bytes(request, bytes).and_then(|frame| match frame {
        ResponseFrame::SingleLine { framed, .. } if framed.next_response_input.is_empty() => {
            Ok(&bytes[framed.response])
        }
        ResponseFrame::SingleLine { .. } | ResponseFrame::Multiline { .. } => {
            Err(ResponseReadError::Invalid(SmallVec::new()))
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResponseReadError {
    Incomplete,
    Invalid(smallvec::SmallVec<[ResponseWarning; 0]>),
}

impl ResponseReadError {
    pub(crate) fn log_warnings(
        &self,
        buffer: &[u8],
        client_addr: impl std::fmt::Display,
        backend_id: crate::types::BackendId,
    ) {
        if let Self::Invalid(warnings) = self {
            log_response_warnings(warnings, buffer, client_addr, backend_id);
        }
    }
}

fn log_response_warnings(
    warnings: &[ResponseWarning],
    buffer: &[u8],
    client_addr: impl std::fmt::Display,
    backend_id: crate::types::BackendId,
) {
    use tracing::warn;

    for warning in warnings {
        match warning {
            ResponseWarning::ShortResponse { bytes, min } => {
                warn!(
                    "Client {} got short response from backend {:?} ({} bytes < {} min): {:02x?}",
                    client_addr, backend_id, bytes, min, buffer
                );
            }
            ResponseWarning::InvalidResponse => {
                warn!(
                    client = %client_addr,
                    backend = ?backend_id,
                    first_bytes_hex = %crate::session::backend::format_hex_preview(buffer, 256),
                    first_bytes_utf8 = %String::from_utf8_lossy(&buffer[..buffer.len().min(256)]),
                    "Backend returned invalid response"
                );
            }
            ResponseWarning::UnusualStatusCode(code) => {
                warn!(
                    client = %client_addr,
                    backend = ?backend_id,
                    status_code = code,
                    first_bytes_hex = %crate::session::backend::format_hex_preview(buffer, 256),
                    first_bytes_utf8 = %String::from_utf8_lossy(&buffer[..buffer.len().min(256)]),
                    "Backend returned unusual status code"
                );
            }
        }
    }
}

#[cfg(test)]
pub(crate) async fn capture_isolated_multiline_response(
    conn: &mut crate::stream::ConnectionStream,
    io_buffer: &mut crate::pool::PooledBuffer,
    capture: &mut crate::pool::PooledBuffer,
) -> anyhow::Result<()> {
    IsolatedMultilineResponse::begin(conn, io_buffer)
        .map_err(isolated_multiline_error)?
        .capture_into(capture)
        .await
}

pub(crate) async fn observe_isolated_multiline_response(
    conn: &mut crate::stream::ConnectionStream,
    io_buffer: &mut crate::pool::PooledBuffer,
) -> Result<(), FramingError> {
    IsolatedMultilineResponse::begin(conn, io_buffer)?
        .observe()
        .await
}

pub(crate) async fn capture_isolated_multiline_response_chunked_optional(
    conn: &mut crate::stream::ConnectionStream,
    io_buffer: &mut crate::pool::PooledBuffer,
    pool: &crate::pool::BufferPool,
    response: &mut crate::pool::ChunkedResponse,
) -> Result<Option<crate::cache::CachePayloadEnd>, FramingError> {
    IsolatedMultilineResponse::begin(conn, io_buffer)?
        .capture_chunked_optional(pool, response, MAX_CAPTURED_MULTILINE_RESPONSE_BYTES)
        .await
}

/// The isolated operation rejects packed suffixes and owns its continuation.
/// A read can only advance the scanner belonging to this borrowed input window.
struct IsolatedMultilineResponse<'a> {
    cursor: ResponseCursor<'a>,
}

impl<'a> IsolatedMultilineResponse<'a> {
    fn begin(
        conn: &'a mut crate::stream::ConnectionStream,
        io_buffer: &'a mut crate::pool::PooledBuffer,
    ) -> Result<Self, FramingError> {
        Ok(Self {
            cursor: ResponseCursor::begin(conn, io_buffer, PackedPendingBytesPolicy::Reject)?,
        })
    }

    #[cfg(test)]
    async fn capture_into(self, capture: &mut crate::pool::PooledBuffer) -> anyhow::Result<()> {
        self.capture_into_mode::<true>(capture).await
    }

    async fn capture_payload_into(
        self,
        capture: &mut crate::pool::PooledBuffer,
    ) -> anyhow::Result<()> {
        self.capture_into_mode::<false>(capture).await
    }

    async fn capture_into_mode<const INCLUDE_TERMINATOR: bool>(
        mut self,
        capture: &mut crate::pool::PooledBuffer,
    ) -> anyhow::Result<()> {
        loop {
            match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => {
                    if INCLUDE_TERMINATOR {
                        chunk.extend_capture_from(self.cursor.io_buffer, capture);
                    } else {
                        chunk.extend_payload_capture_from(self.cursor.io_buffer, capture);
                    }
                    return Ok(());
                }
                ResponseWindow::Incomplete(chunk) => {
                    chunk.extend_capture_from(self.cursor.io_buffer, capture);
                }
            }
            // Preserve the underlying I/O error in the fallible capture API.
            let read = self
                .cursor
                .io_buffer
                .read_from(self.cursor.conn)
                .await
                .context("Failed to read multiline response from backend")?;
            if read == 0 {
                return Err(isolated_multiline_error(FramingError::BackendEof));
            }
            self.cursor
                .frame_next_chunk()
                .map_err(isolated_multiline_error)?;
        }
    }

    async fn read_next_chunk(&mut self) -> Result<(), FramingError> {
        let read = self
            .cursor
            .io_buffer
            .read_from(self.cursor.conn)
            .await
            .map_err(|_| FramingError::Io)?;
        if read == 0 {
            return Err(FramingError::BackendEof);
        }
        self.cursor.frame_next_chunk()
    }

    async fn observe(mut self) -> Result<(), FramingError> {
        loop {
            match &self.cursor.frame {
                ResponseWindow::Complete(_) => return Ok(()),
                ResponseWindow::Incomplete(_) => self.read_next_chunk().await?,
            }
        }
    }

    async fn capture_chunked_optional(
        mut self,
        pool: &crate::pool::BufferPool,
        response: &mut crate::pool::ChunkedResponse,
        retention_limit: usize,
    ) -> Result<Option<crate::cache::CachePayloadEnd>, FramingError> {
        loop {
            let chunk_len = match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => chunk.response.len(),
                ResponseWindow::Incomplete(chunk) => chunk.response.len(),
            };
            if capture_would_exceed_limit(response.len(), chunk_len, retention_limit) {
                response.clear();
                self.observe().await?;
                return Ok(None);
            }
            match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => {
                    chunk.push_isolated_buffer_to(response, pool, self.cursor.io_buffer);
                    let payload_end = response
                        .len()
                        .checked_sub(DOT_TERMINATOR.len())
                        .expect("a complete multiline capture includes its terminator");
                    return Ok(crate::cache::CachePayloadEnd::new(
                        payload_end,
                        response.len(),
                    ));
                }
                ResponseWindow::Incomplete(chunk) => {
                    chunk.push_buffer_to(response, pool, self.cursor.io_buffer);
                    self.read_next_chunk().await?;
                }
            }
        }
    }
}

#[cfg(test)]
async fn capture_response(
    request: &crate::protocol::RequestContext,
    io_buffer: crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    response: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
    ClassifiedResponse::read_for_test(conn, request, io_buffer)
        .await
        .map_err(crate::session::response_transfer::ResponseTransferError::Io)?
        .stream_for_test(conn, pool, backend_id)?
        .capture(response)
        .await
}

/// One response operation owns the matching scanner and exclusive I/O window.
/// No continuation or response range escapes independently of these resources.
struct StreamingResponse<'a> {
    cursor: ResponseCursor<'a>,
    pool: &'a crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
    metadata: ResponseMetadata,
    shape: ResponseShape,
    bytes_received: u64,
}

#[derive(Clone, Copy)]
struct ResponseMetadata {
    kind: crate::protocol::RequestKind,
    status: crate::protocol::StatusCode,
    status_line_end: crate::protocol::StatusLineEnd,
}

enum ResponseShape {
    SingleLine,
    Multiline,
}

impl<'a> StreamingResponse<'a> {
    fn is_complete(&self) -> bool {
        matches!(self.cursor.frame, ResponseWindow::Complete(_))
    }

    fn from_classification(
        kind: crate::protocol::RequestKind,
        classification: Result<ResponseFrame, ResponseReadError>,
        io_buffer: &'a mut crate::pool::PooledBuffer,
        conn: &'a mut crate::stream::ConnectionStream,
        pool: &'a crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> Result<Self, crate::session::response_transfer::ResponseTransferError> {
        let (shape, metadata, cursor) = match classification.map_err(|err| {
            crate::session::response_transfer::ResponseTransferError::Io(anyhow::anyhow!(
                "Failed to frame response: {err:?}"
            ))
        })? {
            ResponseFrame::SingleLine {
                status,
                status_line_end,
                framed,
            } => (
                ResponseShape::SingleLine,
                ResponseMetadata {
                    kind,
                    status,
                    status_line_end,
                },
                ResponseCursor {
                    conn,
                    io_buffer,
                    framer: MultilineFramer::default(),
                    frame: ResponseWindow::Complete(CompleteResponseWindow {
                        capture: framed.response.clone(),
                        response: framed.response,
                        next_response_input: framed.next_response_input,
                    }),
                    packed_policy: PackedPendingBytesPolicy::AllowIfStatusPrefix,
                },
            ),
            ResponseFrame::Multiline {
                status,
                status_line_end,
            } => (
                ResponseShape::Multiline,
                ResponseMetadata {
                    kind,
                    status,
                    status_line_end,
                },
                ResponseCursor::begin_after_status_line(
                    conn,
                    io_buffer,
                    status_line_end,
                    PackedPendingBytesPolicy::AllowIfStatusPrefix,
                )
                .map_err(|error| {
                    crate::session::response_transfer::ResponseTransferError::Io(
                        isolated_multiline_error(error),
                    )
                })?,
            ),
        };
        let bytes_received = cursor.io_buffer.initialized() as u64;
        Ok(Self {
            cursor,
            pool,
            backend_id,
            metadata,
            shape,
            bytes_received,
        })
    }

    /// Only newly appended bytes enter the scanner. Translation back into the
    /// visible window happens while the append result still borrows that window.
    async fn append_to_retained_prefix_if_writable(
        &mut self,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        match &self.cursor.frame {
            ResponseWindow::Complete(_) => return Ok(()),
            ResponseWindow::Incomplete(_) => {}
        };
        let Some(permit) = self.cursor.io_buffer.retained_append_permit() else {
            return Ok(());
        };
        let appended = match permit.read(self.cursor.conn).await.map_err(|error| {
            crate::session::response_transfer::ResponseTransferError::Io(
                anyhow::Error::from(error).context("Failed to read remaining response body"),
            )
        })? {
            crate::pool::AppendOutcome::Data(appended) => appended,
            crate::pool::AppendOutcome::Eof(buffer) => {
                let _ = buffer;
                return Err(
                    crate::session::response_transfer::ResponseTransferError::BackendEof {
                        backend_id: self.backend_id,
                        bytes_received: self.bytes_received,
                    },
                );
            }
        };
        let new_len = appended.as_new_bytes().len();
        self.bytes_received += new_len as u64;
        self.cursor.frame = self
            .cursor
            .framer
            .split_appended(appended, self.cursor.packed_policy)
            .expect("pending bytes policy cannot reject trailing bytes");
        Ok(())
    }

    async fn read_next_chunk(
        &mut self,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        let read = self
            .cursor
            .io_buffer
            .read_from(self.cursor.conn)
            .await
            .map_err(|error| {
                crate::session::response_transfer::ResponseTransferError::Io(
                    anyhow::Error::from(error).context("Failed to read remaining response body"),
                )
            })?;
        if read == 0 {
            return Err(
                crate::session::response_transfer::ResponseTransferError::BackendEof {
                    backend_id: self.backend_id,
                    bytes_received: self.bytes_received,
                },
            );
        }
        self.bytes_received += read as u64;
        self.cursor.frame_next_chunk().map_err(|error| {
            crate::session::response_transfer::ResponseTransferError::Io(isolated_multiline_error(
                error,
            ))
        })
    }

    async fn drain(
        &mut self,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        loop {
            match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => {
                    return chunk
                        .queue_next_response_input(self.cursor.io_buffer, self.cursor.conn);
                }
                ResponseWindow::Incomplete(_) => self.read_next_chunk().await?,
            }
        }
    }

    async fn observe(
        &mut self,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        match &self.cursor.frame {
            ResponseWindow::Complete(chunk) => {
                chunk.observe_from_buffer(self.cursor.io_buffer, self.cursor.conn, self.pool)
            }
            ResponseWindow::Incomplete(_) => self.drain().await,
        }
    }

    async fn write<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
    ) -> Result<ResponseWriteStats, crate::session::response_transfer::ResponseTransferError> {
        self.append_to_retained_prefix_if_writable().await?;
        self.write_current_and_remaining_chunks(writer).await
    }

    async fn write_current_and_remaining_chunks<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
    ) -> Result<ResponseWriteStats, crate::session::response_transfer::ResponseTransferError> {
        let mut stats = ResponseWriteStats::default();
        loop {
            match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => {
                    stats += chunk
                        .write_from(writer, self.cursor.io_buffer, self.cursor.conn, self.pool)
                        .await?;
                    return Ok(stats);
                }
                ResponseWindow::Incomplete(chunk) => {
                    let bytes = &self.cursor.io_buffer[chunk.response.clone()];
                    if let Err(error) = writer.write_all(bytes).await {
                        self.drain().await?;
                        return Err(crate::session::response_transfer::ResponseTransferError::ClientDisconnect(error));
                    }
                    stats.add_chunk(bytes.len());
                    self.read_next_chunk().await?;
                }
            }
        }
    }

    async fn capture_and_write<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        response: &mut crate::pool::ChunkedResponse,
        retention_limit: usize,
    ) -> Result<
        (u64, Option<CapturedChunkedResponse>),
        crate::session::response_transfer::ResponseTransferError,
    > {
        response.clear();
        loop {
            let chunk_len = match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => chunk.response.len(),
                ResponseWindow::Incomplete(chunk) => chunk.response.len(),
            };
            let exceeds_limit = match self.shape {
                ResponseShape::SingleLine => false,
                ResponseShape::Multiline => {
                    capture_would_exceed_limit(response.len(), chunk_len, retention_limit)
                }
            };
            if exceeds_limit {
                let mut stats = ResponseWriteStats::default();
                stats.add_buffered_response(response);
                if let Err(error) = response.write_all_to_recording(writer).await {
                    self.observe().await?;
                    return Err(
                        crate::session::response_transfer::ResponseTransferError::ClientDisconnect(
                            error,
                        ),
                    );
                }
                response.clear();
                stats += self.write_current_and_remaining_chunks(writer).await?;
                stats.record();
                return Ok((stats.bytes_written_u64(), None));
            }

            match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => {
                    chunk.push_from_buffer(
                        self.cursor.io_buffer,
                        self.cursor.conn,
                        response,
                        self.pool,
                    )?;
                    let mut stats = ResponseWriteStats::default();
                    stats.add_buffered_response(response);
                    response.write_all_to_recording(writer).await.map_err(
                        crate::session::response_transfer::ResponseTransferError::ClientDisconnect,
                    )?;
                    stats.record();
                    let payload_end = match self.shape {
                        ResponseShape::SingleLine => response.len(),
                        ResponseShape::Multiline => response
                            .len()
                            .checked_sub(DOT_TERMINATOR.len())
                            .expect("a complete multiline capture includes its terminator"),
                    };
                    let payload_end =
                        crate::cache::CachePayloadEnd::new(payload_end, response.len())
                            .expect("framer established a cache payload within the capture");
                    let captured = CapturedChunkedResponse::from_capture(
                        self.metadata.kind,
                        self.metadata.status,
                        self.metadata.status_line_end,
                        std::mem::take(response),
                        payload_end,
                    );
                    return Ok((stats.bytes_written_u64(), Some(captured)));
                }
                ResponseWindow::Incomplete(chunk) => {
                    chunk.push_buffer_to(response, self.pool, self.cursor.io_buffer);
                    self.read_next_chunk().await?;
                }
            }
        }
    }

    #[cfg(test)]
    async fn capture(
        mut self,
        response: &mut crate::pool::ChunkedResponse,
    ) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
        response.clear();
        loop {
            let complete = match &self.cursor.frame {
                ResponseWindow::Complete(chunk) => {
                    chunk.push_from_buffer(
                        self.cursor.io_buffer,
                        self.cursor.conn,
                        response,
                        self.pool,
                    )?;
                    true
                }
                ResponseWindow::Incomplete(chunk) => {
                    chunk.push_buffer_to(response, self.pool, self.cursor.io_buffer);
                    false
                }
            };
            match self.shape {
                ResponseShape::SingleLine => {}
                ResponseShape::Multiline => {
                    ensure_capture_len(response.len()).map_err(|error| {
                        crate::session::response_transfer::ResponseTransferError::Io(
                            isolated_multiline_error(error),
                        )
                    })?
                }
            }
            if complete {
                return Ok(());
            }
            self.read_next_chunk().await?;
        }
    }
}

#[cfg(test)]
async fn write_response<W: AsyncWrite + Unpin>(
    request: &crate::protocol::RequestContext,
    buffer: crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    writer: &mut W,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<u64, crate::session::response_transfer::ResponseTransferError> {
    ClassifiedResponse::read_for_test(conn, request, buffer)
        .await
        .map_err(crate::session::response_transfer::ResponseTransferError::Io)?
        .stream_for_test(conn, pool, backend_id)
        .expect("classified response should stream")
        .write(writer)
        .await
        .map(|stats| {
            stats.record();
            stats.bytes_written_u64()
        })
}

impl MultilineFramer {
    fn frame_with_policy(
        &mut self,
        chunk: &[u8],
        suffix_policy: PackedPendingBytesPolicy,
    ) -> Result<ResponseWindow, FramingError> {
        self.split_chunk(chunk, suffix_policy)
            .map(|progress| progress.in_window(WindowOffset::new(0), chunk.len()))
    }

    #[cfg(test)]
    fn frame_multiline_chunk(&mut self, chunk: &[u8]) -> ResponseWindow {
        self.frame_with_policy(chunk, PackedPendingBytesPolicy::AllowIfStatusPrefix)
            .expect("pending bytes policy cannot reject trailing bytes")
    }

    /// Frame bytes appended to a retained response window.
    ///
    /// The append result owns the same buffer that supplied the bytes and
    /// records the logical end before the read. Consuming it prevents a
    /// stale appended view from surviving the framing transition. Keeping
    /// coordinate translation here also prevents a caller from pairing an
    /// arbitrary origin with an unrelated appended slice.
    fn split_appended(
        &mut self,
        appended: crate::pool::buffer::AppendedRead<'_>,
        suffix_policy: PackedPendingBytesPolicy,
    ) -> Result<ResponseWindow, FramingError> {
        let origin = WindowOffset::new(appended.previous_len());
        let new_bytes = appended.as_new_bytes();
        self.split_chunk(new_bytes, suffix_policy)
            .map(|progress| progress.in_window(origin, origin.get() + new_bytes.len()))
    }

    /// Update tail with the last bytes from a chunk
    ///
    /// Maintains the last `TERMINATOR_TAIL_SIZE` bytes of the concatenation of
    /// all prior chunks. When `chunk` is smaller than `TERMINATOR_TAIL_SIZE`,
    /// the prior tail bytes are shifted to preserve the rolling window — not
    /// overwritten — so terminators split across three or more tiny reads
    /// (e.g. `\r\n`, `.`, `\r\n`) are correctly detected.
    fn update(&mut self, chunk: &[u8]) {
        if chunk.len() >= TERMINATOR_TAIL_SIZE {
            // Chunk alone fills the window — take its last N bytes
            self.data
                .copy_from_slice(&chunk[chunk.len() - TERMINATOR_TAIL_SIZE..]);
            self.len = TERMINATOR_TAIL_SIZE;
        } else if !chunk.is_empty() {
            let combined_len = self.len + chunk.len();
            if combined_len >= TERMINATOR_TAIL_SIZE {
                // Shift prior tail left to keep window full, then append chunk
                let keep = TERMINATOR_TAIL_SIZE - chunk.len();
                self.data.copy_within(self.len - keep..self.len, 0);
                self.data[keep..keep + chunk.len()].copy_from_slice(chunk);
                self.len = TERMINATOR_TAIL_SIZE;
            } else {
                // Combined bytes still fit — just append
                self.data[self.len..self.len + chunk.len()].copy_from_slice(chunk);
                self.len = combined_len;
            }
        }
    }

    fn split_chunk(
        &mut self,
        chunk: &[u8],
        suffix_policy: PackedPendingBytesPolicy,
    ) -> Result<ChunkProgress, FramingError> {
        if let Some(end) = self.find_spanning_terminator(chunk) {
            if end == chunk.len()
                || matches!(suffix_policy, PackedPendingBytesPolicy::AllowIfStatusPrefix)
                    && plausible_status_prefix(&chunk[end..])
            {
                return Ok(ChunkProgress::Complete(CompleteChunk {
                    consumed: ChunkConsumed(end),
                    payload_end: ChunkPayloadEnd(0),
                }));
            }
            if matches!(suffix_policy, PackedPendingBytesPolicy::Reject) {
                return Err(FramingError::UnexpectedTrailingResponseBytes);
            }
        }
        for end in terminator_ends_in_chunk(chunk) {
            if end == chunk.len() {
                return Ok(ChunkProgress::Complete(CompleteChunk {
                    consumed: ChunkConsumed(end),
                    payload_end: ChunkPayloadEnd(
                        end.checked_sub(TERMINATOR.len())
                            .expect("complete multiline response includes its terminator"),
                    ),
                }));
            }

            match suffix_policy {
                PackedPendingBytesPolicy::Reject => {
                    return Err(FramingError::UnexpectedTrailingResponseBytes);
                }
                PackedPendingBytesPolicy::AllowIfStatusPrefix
                    if plausible_status_prefix(&chunk[end..]) =>
                {
                    return Ok(ChunkProgress::Complete(CompleteChunk {
                        consumed: ChunkConsumed(end),
                        payload_end: ChunkPayloadEnd(
                            end.checked_sub(TERMINATOR.len())
                                .expect("complete multiline response includes its terminator"),
                        ),
                    }));
                }
                PackedPendingBytesPolicy::AllowIfStatusPrefix => {}
            }
        }

        self.update(chunk);
        Ok(ChunkProgress::Incomplete)
    }

    /// Find spanning terminator offset in chunk
    ///
    /// Returns the byte offset in the chunk where the terminator ends,
    /// or None if no spanning terminator is found.
    #[must_use]
    fn find_spanning_terminator(&self, chunk: &[u8]) -> Option<usize> {
        // Early return if buffer is empty - no boundary to span
        if self.len == 0 {
            return None;
        }
        find_spanning_terminator(&self.data[..self.len], self.len, chunk, chunk.len())
    }

    /// Return the earliest terminator end offset touching the current chunk.
    ///
    /// Unlike [`find_terminator_end`], this includes terminators split across the
    /// framer's prior tail and `chunk`.
    #[must_use]
    #[cfg(test)]
    fn next_terminator_end(&self, chunk: &[u8]) -> Option<usize> {
        // A spanning hit always ends within the first 4 bytes of `chunk`, while an
        // in-chunk terminator must end at byte 5 or later, so spanning-first is
        // also earliest-first.
        self.find_spanning_terminator(chunk)
            .or_else(|| find_terminator_end_from(chunk, 0))
    }

    /// Return the earliest terminator end offset, updating rolling state on miss.
    ///
    /// If no terminator is found, this appends `chunk` into the framer's rolling
    /// tail so a split terminator can be detected when the next chunk arrives.
    ///
    /// If a terminator is found, the framer is intentionally left unchanged. Callers
    /// should stop using this framer instance after `Some(_)` and start a fresh one
    /// for the next response.
    #[must_use]
    #[cfg(test)]
    fn advance_to_next_terminator_end(&mut self, chunk: &[u8]) -> Option<usize> {
        let pos = self.next_terminator_end(chunk);
        if pos.is_none() {
            self.update(chunk);
        }
        pos
    }
}

impl BackendReplyTracker {
    fn push_request(&mut self, kind: crate::protocol::RequestKind) {
        self.pending.push_back(PendingRequestFrame {
            kind,
            state: PendingRequestFrameState::AwaitingStatusLine,
            status_line: smallvec::SmallVec::new(),
        });
    }

    fn accept_backend_bytes<'a>(
        &mut self,
        chunk: &'a [u8],
    ) -> smallvec::SmallVec<[BackendReplyBytes<'a>; 4]> {
        let mut output = smallvec::SmallVec::new();
        let mut offset = BackendChunkEnd::new(0);

        while offset.get() < chunk.len() {
            let Some(front) = self.pending.front_mut() else {
                output.push(BackendReplyBytes::ForwardUntracked(&chunk[offset.get()..]));
                break;
            };
            let Some(framed) = front.consume(chunk, offset) else {
                output.push(BackendReplyBytes::ForwardUntracked(&chunk[offset.get()..]));
                break;
            };

            offset = framed.end();
            output.push(BackendReplyBytes::CompletedTrackedReply(
                framed.backend_bytes(chunk),
            ));
            self.pending.pop_front();
        }

        output
    }
}

impl PendingRequestFrame {
    fn consume(
        &mut self,
        chunk: &[u8],
        offset: BackendChunkEnd,
    ) -> Option<FramedResponseForRequest> {
        let offset_bytes = &chunk[offset.get()..];
        match &mut self.state {
            PendingRequestFrameState::AwaitingStatusLine => {
                let Some(pos) = memchr::memchr(b'\n', offset_bytes) else {
                    if self.status_line.len() + offset_bytes.len()
                        > crate::constants::buffer::COMMAND
                    {
                        return Some(FramedResponseForRequest {
                            response: BackendChunkRange::new(
                                offset,
                                BackendChunkEnd::new(chunk.len()),
                            ),
                        });
                    }
                    self.status_line.extend_from_slice(offset_bytes);
                    return None;
                };
                let end = offset.after(ChunkConsumed(pos + 1));
                if self.status_line.len() + end.get() - offset.get()
                    > crate::constants::buffer::COMMAND
                {
                    return Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(offset, end),
                    });
                }
                self.status_line
                    .extend_from_slice(&chunk[offset.get()..end.get()]);
                let Some(status) = crate::protocol::StatusCode::parse(self.status_line.as_slice())
                else {
                    return Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(offset, end),
                    });
                };
                if !crate::protocol::request_kind_has_response_body(self.kind, status) {
                    return Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(offset, end),
                    });
                }

                let mut framer = MultilineFramer::default();
                framer.update(self.status_line.as_slice());
                self.status_line.clear();
                match framer.split_chunk(
                    &chunk[end.get()..],
                    PackedPendingBytesPolicy::AllowIfStatusPrefix,
                ) {
                    Ok(ChunkProgress::Complete(complete)) => Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(
                            offset,
                            BackendChunkEnd::new(end.get()).after(complete.consumed),
                        ),
                    }),
                    Ok(ChunkProgress::Incomplete) => {
                        self.state = PendingRequestFrameState::ReadingMultiline { framer };
                        None
                    }
                    Err(_) => Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(offset, BackendChunkEnd::new(chunk.len())),
                    }),
                }
            }
            PendingRequestFrameState::ReadingMultiline { framer } => {
                match framer
                    .split_chunk(offset_bytes, PackedPendingBytesPolicy::AllowIfStatusPrefix)
                {
                    Ok(ChunkProgress::Complete(complete)) => Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(offset, offset.after(complete.consumed)),
                    }),
                    Ok(ChunkProgress::Incomplete) => None,
                    Err(_) => Some(FramedResponseForRequest {
                        response: BackendChunkRange::new(offset, BackendChunkEnd::new(chunk.len())),
                    }),
                }
            }
        }
    }
}

fn plausible_status_prefix(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if !matches!(bytes[0], b'1'..=b'5') {
        return false;
    }
    match status_line_len(bytes) {
        Some(end) => crate::protocol::StatusCode::parse(&bytes[..end]).is_some(),
        None => bytes.iter().take(3).all(u8::is_ascii_digit),
    }
}

fn status_line_len(bytes: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', bytes).map(|pos| pos + 1)
}

fn isolated_multiline_error(err: FramingError) -> anyhow::Error {
    match err {
        FramingError::UnexpectedTrailingResponseBytes => {
            anyhow::anyhow!("Backend sent unexpected trailing bytes after multiline response")
        }
        FramingError::InvalidFrameMetadata => {
            anyhow::anyhow!("Framer received inconsistent response metadata")
        }
        FramingError::BackendEof => {
            anyhow::anyhow!("Backend closed connection before complete multiline response")
        }
        FramingError::Io => anyhow::anyhow!("Failed to read multiline response from backend"),
        FramingError::CapturedResponseTooLarge { bytes, max } => anyhow::anyhow!(
            "Captured multiline response exceeded retention limit ({bytes} bytes > {max} bytes)"
        ),
    }
}

#[cfg(test)]
fn ensure_capture_len(len: usize) -> Result<(), FramingError> {
    ensure_capture_len_with_limit(len, MAX_CAPTURED_MULTILINE_RESPONSE_BYTES)
}

fn ensure_capture_len_with_limit(len: usize, max: usize) -> Result<(), FramingError> {
    if len > max {
        return Err(FramingError::CapturedResponseTooLarge { bytes: len, max });
    }
    Ok(())
}

fn capture_would_exceed_limit(current_len: usize, next_len: usize, max: usize) -> bool {
    ensure_capture_len_with_limit(current_len.saturating_add(next_len), max).is_err()
}

#[must_use]
fn complete_multiline_payload_split(payload: &[u8]) -> Option<CompleteMultilinePayloadSplit> {
    if payload == b".\r\n" {
        return Some(CompleteMultilinePayloadSplit::new(0..0, 0..3));
    }
    let mut framer = MultilineFramer::default();
    match framer.split_chunk(payload, PackedPendingBytesPolicy::Reject) {
        Ok(ChunkProgress::Complete(complete)) => {
            let response_end = complete.consumed.0;
            let body_end = response_end.checked_sub(DOT_TERMINATOR.len())?;
            let terminator_start = response_end.checked_sub(DOT_TERMINATOR.len())?;
            Some(CompleteMultilinePayloadSplit::new(
                0..body_end,
                terminator_start..response_end,
            ))
        }
        Ok(ChunkProgress::Incomplete) | Err(_) => None,
    }
}

#[must_use]
fn complete_multiline_payload_body(payload: &[u8]) -> Option<&[u8]> {
    complete_multiline_payload_split(payload).map(|split| &payload[split.body()])
}

/// Returns the captured semantic payload, retaining its final content CRLF and
/// excluding only the dot terminator.
#[must_use]
pub(crate) fn captured_multiline_payload_body(payload: &[u8]) -> Option<&[u8]> {
    complete_multiline_payload_body(payload)
}

/// Find the position of the NNTP multiline terminator in data
///
/// Returns the position AFTER the terminator (exclusive end), or None if not found.
/// This handles the case where extra data appears after the terminator in the same chunk.
///
/// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
/// the terminator is exactly "\r\n.\r\n" (CRLF, dot, CRLF).
///
/// Returns the earliest complete terminator when multiple responses are packed
/// into the same read buffer, after scanning the full buffer for terminators.
///
/// Never optimize this by checking only whether the buffer ends with the
/// terminator. A read buffer can contain multiple complete multiline responses
/// or payload bytes before a later terminator-shaped suffix. The first complete
/// terminator is the protocol boundary.
#[inline]
#[cfg(test)]
fn find_terminator_end(data: &[u8]) -> Option<usize> {
    find_terminator_end_from(data, 0)
}

#[inline]
fn terminator_ends_in_chunk(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    TERMINATOR_FINDER
        .find_iter(data)
        .map(|found| found + TERMINATOR.len())
}

#[cfg(feature = "framing-bench")]
#[must_use]
pub fn benchmark_multiline_response(body_len: usize) -> Vec<u8> {
    let mut response = b"220 42 <benchmark@example.com>\r\n".to_vec();
    response.extend(std::iter::repeat_n(b'x', body_len));
    response.extend_from_slice(TERMINATOR);
    response
}

/// Measure the production-path baseline while retaining only its rolling tail.
#[cfg(feature = "framing-bench")]
#[must_use]
pub fn benchmark_incremental_multiline_frame(response: &[u8], chunk_size: usize) -> usize {
    let mut framer = MultilineFramer::default();
    let chunk_size = chunk_size.max(1);

    for chunk in response.chunks(chunk_size) {
        if let Ok(ChunkProgress::Complete(complete)) =
            framer.split_chunk(chunk, PackedPendingBytesPolicy::AllowIfStatusPrefix)
        {
            return complete.consumed.0;
        }
    }

    0
}

/// Measure the control path that repeats a full accumulated-response rescan.
#[cfg(feature = "framing-bench")]
#[must_use]
pub fn benchmark_stateless_multiline_frame(response: &[u8], chunk_size: usize) -> usize {
    let chunk_size = chunk_size.max(1);
    let mut received = 0;

    for chunk in response.chunks(chunk_size) {
        received += chunk.len();
        let mut framer = MultilineFramer::default();
        if let Ok(ChunkProgress::Complete(complete)) = framer.split_chunk(
            &response[..received],
            PackedPendingBytesPolicy::AllowIfStatusPrefix,
        ) {
            return complete.consumed.0;
        }
    }

    0
}

/// Build the same framer-bounded cache input that production capture hands to
/// the cache adapter.
///
/// This benchmark-only adapter keeps status classification, chunk progression,
/// terminator recognition, and payload-boundary derivation inside this module.
/// The cache benchmark therefore measures the production framed-ingest API
/// without fabricating a `FramedArticleState` from caller-supplied offsets.
#[cfg(feature = "framing-bench")]
#[must_use]
pub fn benchmark_framed_cache_response(
    response: &[u8],
    kind: crate::protocol::RequestKind,
    chunk_size: usize,
) -> crate::cache::FramedChunkedResponse {
    assert!(chunk_size > 0, "benchmark chunk size must be nonzero");
    let status_line_end = status_line_len(response).expect("benchmark response status line");
    let status = parse_response_status(response)
        .status_code
        .expect("benchmark response status code");
    let multiline = crate::protocol::request_kind_has_response_body(kind, status);

    let mut response_end = status_line_end;
    if multiline {
        let mut framer = MultilineFramer::default();
        framer.update(&response[..status_line_end]);
        let mut complete = false;
        for chunk in response[status_line_end..].chunks(chunk_size) {
            match framer
                .split_chunk(chunk, PackedPendingBytesPolicy::AllowIfStatusPrefix)
                .expect("benchmark response must satisfy packed-byte policy")
            {
                ChunkProgress::Incomplete => response_end += chunk.len(),
                ChunkProgress::Complete(progress) => {
                    response_end += progress.consumed.0;
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete, "benchmark response is incomplete");
        assert_eq!(
            response_end,
            response.len(),
            "benchmark response has a suffix"
        );
    }

    let pool = crate::pool::BufferPool::new(
        crate::types::BufferSize::try_new(chunk_size).expect("benchmark chunk size is valid"),
        1,
    )
    .with_capture_pool(chunk_size, response.len().div_ceil(chunk_size).max(1));
    let mut captured = crate::pool::ChunkedResponse::default();
    for chunk in response.chunks(chunk_size) {
        let mut buffer = pool.acquire_capture();
        buffer.copy_from_slice(chunk);
        captured.push_buffer_range(buffer, 0..chunk.len());
    }

    let payload_end = if multiline {
        response
            .len()
            .checked_sub(DOT_TERMINATOR.len())
            .expect("multiline benchmark response terminator")
    } else {
        status_line_end
    };
    let state = crate::protocol::ArticleState::new(crate::protocol::FramedArticleState::new(
        captured,
        kind,
        status,
        crate::protocol::StatusLineEnd::new(status_line_end),
        crate::protocol::ContentEnd::new(payload_end),
    ));
    crate::cache::FramedChunkedResponse::from_article_state(state)
}

#[inline]
#[cfg(test)]
fn find_terminator_end_from(data: &[u8], start: usize) -> Option<usize> {
    let data = data.get(start..)?;
    let mut first = None;
    for end in terminator_ends_in_chunk(data) {
        first.get_or_insert(start + end);
    }
    first
}

/// Find spanning terminator across boundary between tail and current chunk
///
/// Returns the byte offset in the current chunk where the terminator ends,
/// or None if no spanning terminator is found.
///
/// This handles the case where a multiline terminator is split across two read chunks.
/// For example: previous chunk ends with "\r\n." and current starts with "\r\n" → returns Some(2)
///
/// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
/// the terminator is exactly "\r\n.\r\n" (CRLF, dot, CRLF).
#[inline]
fn find_spanning_terminator(
    tail: &[u8],
    tail_len: usize,
    current: &[u8],
    current_len: usize,
) -> Option<usize> {
    if tail_len < 1 || current_len < 1 {
        return None;
    }

    // Check all possible split positions of the 5-byte terminator "\r\n.\r\n"
    // Split after byte 1: tail ends with "\r", current starts with "\n.\r\n" → offset 4
    if tail_len >= 1
        && current_len >= 4
        && tail[tail_len - 1] == b'\r'
        && current[..4] == *b"\n.\r\n"
    {
        return Some(4);
    }
    // Split after byte 2: tail ends with "\r\n", current starts with ".\r\n" → offset 3
    if tail_len >= 2
        && current_len >= 3
        && tail[tail_len - 2..tail_len] == *b"\r\n"
        && current[..3] == *b".\r\n"
    {
        return Some(3);
    }
    // Split after byte 3: tail ends with "\r\n.", current starts with "\r\n" → offset 2
    if tail_len >= 3
        && current_len >= 2
        && tail[tail_len - 3..tail_len] == *b"\r\n."
        && current[..2] == *b"\r\n"
    {
        return Some(2);
    }
    // Split after byte 4: tail ends with "\r\n.\r", current starts with "\n" → offset 1
    if tail_len >= 4
        && current_len >= 1
        && tail[tail_len - 4..tail_len] == *b"\r\n.\r"
        && current[0] == b'\n'
    {
        return Some(1);
    }

    None
}

#[cfg(test)]
mod tests {
    mod contract_fixtures {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/response_contract.rs"
        ));
    }

    use super::*;
    use crate::session::response_transfer::ResponseTransferError;
    use crate::types::BufferSize;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn request_kind(shape: contract_fixtures::RequestShape) -> crate::protocol::RequestKind {
        match shape {
            contract_fixtures::RequestShape::Article => crate::protocol::RequestKind::Article,
            contract_fixtures::RequestShape::Body => crate::protocol::RequestKind::Body,
            contract_fixtures::RequestShape::Head => crate::protocol::RequestKind::Head,
            contract_fixtures::RequestShape::Stat => crate::protocol::RequestKind::Stat,
            contract_fixtures::RequestShape::Group => crate::protocol::RequestKind::Group,
            contract_fixtures::RequestShape::ListGroup => crate::protocol::RequestKind::ListGroup,
        }
    }

    const EXHAUSTIVE_BYTES: [u8; 4] = *b"\r\n.x";

    struct FailingWriter;

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        scalar_writes: usize,
        vectored_writes: usize,
    }

    #[test]
    fn captured_response_limit_allows_boundary_and_rejects_excess() {
        assert!(ensure_capture_len(MAX_CAPTURED_MULTILINE_RESPONSE_BYTES).is_ok());
        assert_eq!(
            ensure_capture_len(MAX_CAPTURED_MULTILINE_RESPONSE_BYTES + 1),
            Err(FramingError::CapturedResponseTooLarge {
                bytes: MAX_CAPTURED_MULTILINE_RESPONSE_BYTES + 1,
                max: MAX_CAPTURED_MULTILINE_RESPONSE_BYTES,
            })
        );
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "client closed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.scalar_writes += 1;
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.vectored_writes += 1;
            let written = bufs.iter().map(|buf| buf.len()).sum();
            for buf in bufs {
                self.bytes.extend_from_slice(buf);
            }
            Poll::Ready(Ok(written))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn make_pool() -> crate::pool::BufferPool {
        crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2)
    }

    #[test]
    fn shared_response_contract_cases_match_production_tracker() {
        for case in contract_fixtures::CASES {
            let mut packed = case.response.to_vec();
            packed.extend_from_slice(case.suffix);

            for chunk_bytes in 1..=packed.len() {
                let mut tracker = BackendReplyTracker::default();
                tracker.push_request(request_kind(case.request));
                if let Some(next_request) = case.suffix_request {
                    tracker.push_request(request_kind(next_request));
                }

                let mut forwarded = Vec::new();
                let mut completed_count = 0;
                for chunk in packed.chunks(chunk_bytes) {
                    for output in tracker.accept_backend_bytes(chunk) {
                        match output {
                            BackendReplyBytes::CompletedTrackedReply(bytes) => {
                                completed_count += 1;
                                forwarded.extend_from_slice(bytes);
                            }
                            BackendReplyBytes::ForwardUntracked(bytes) => {
                                forwarded.extend_from_slice(bytes);
                            }
                        }
                    }
                }

                match case.disposition {
                    contract_fixtures::Disposition::Complete => {
                        assert_eq!(
                            forwarded, packed,
                            "{} at chunk size {chunk_bytes}",
                            case.name
                        );
                        assert_eq!(
                            completed_count,
                            1 + usize::from(case.suffix_request.is_some()),
                            "{} completion count at chunk size {chunk_bytes}",
                            case.name
                        );
                    }
                    contract_fixtures::Disposition::MalformedStatus => {
                        assert_eq!(
                            forwarded, case.response,
                            "{} at chunk size {chunk_bytes}",
                            case.name
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn isolated_single_line_reply_rejects_a_packed_next_response() {
        let pool = make_pool();
        let request = crate::protocol::RequestContext::from_verb_args(b"STAT", b"<first>");
        let mut buffer = pool.acquire();
        buffer.copy_from_slice(b"223 0 <first>\r\n223 0 <next>\r\n");
        let response = ClassifiedResponse::read_for_test(&mut tokio::io::empty(), &request, buffer)
            .await
            .unwrap();
        assert_eq!(
            response.single_line_bytes(),
            Some(b"223 0 <first>\r\n".as_slice())
        );
        assert!(
            response.require_isolated_single_line().is_err(),
            "an isolated request must not authorize reuse with a following reply in its buffer"
        );
    }

    #[tokio::test]
    async fn classified_forwarding_preserves_shape_and_suffix_at_every_split() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"STAT <first>\r\n", b"223 0 <first>\r\n"),
            (b"BODY <first>\r\n", b"430 not found\r\n"),
            (b"BODY <first>\r\n", b"222 0 <first>\r\n.\r\n"),
            (
                b"BODY <first>\r\n",
                b"222 0 <first>\r\n..dot\r\nbody\r\n.\r\n",
            ),
            (
                b"HEAD <first>\r\n",
                b"221 0 <first>\r\nSubject: folded\r\n continuation\r\n.\r\n",
            ),
            (b"GROUP alt.test\r\n", b"211 1 1 1 alt.test\r\n"),
            (
                b"LISTGROUP alt.test\r\n",
                b"211 1 1 1 alt.test\r\n1\r\n.\r\n",
            ),
        ];
        let next = b"223 0 <next>\r\n";
        for &(command, wire) in cases {
            let request = crate::protocol::RequestContext::parse(command).unwrap();
            for split in 0..=wire.len() {
                let pool = make_pool();
                let mut buffer = pool.acquire();
                buffer.copy_from_slice(&wire[..split]);
                let tail = [wire[split..].as_ref(), next.as_slice()].concat();
                let mut conn = mock_backend_conn(vec![tail]).await;
                let mut response = ClassifiedResponse::read_for_test(&mut conn, &request, buffer)
                    .await
                    .unwrap();
                let mut writer = RecordingWriter::default();
                let bytes = response
                    .stream_for_test(&mut conn, &pool, crate::types::BackendId::from_index(0))
                    .unwrap()
                    .write(&mut writer)
                    .await
                    .unwrap()
                    .bytes_written_u64();
                assert_eq!(
                    bytes,
                    wire.len() as u64,
                    "command={command:?}, split={split}"
                );
                assert_eq!(writer.bytes, wire, "command={command:?}, split={split}");
                let mut following = [0; 14];
                conn.read_exact(&mut following).await.unwrap();
                assert_eq!(&following, next, "command={command:?}, split={split}");
            }
        }
    }

    async fn loopback_connection_stream() -> crate::stream::ConnectionStream {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("loopback addr");
        let client = tokio::spawn(async move {
            tokio::net::TcpStream::connect(addr)
                .await
                .expect("connect loopback client")
        });
        let (server, _) = listener.accept().await.expect("accept loopback client");
        let _client = client.await.expect("client task");
        crate::stream::ConnectionStream::plain(server)
    }

    async fn capture_response_for_test(
        conn: &mut crate::stream::ConnectionStream,
        first_chunk: &[u8],
        request_line: &[u8],
        pool: &crate::pool::BufferPool,
    ) -> Result<crate::pool::ChunkedResponse, ResponseTransferError> {
        let request = crate::protocol::RequestContext::parse(request_line)
            .expect("test request should parse");
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(first_chunk);
        let mut captured = crate::pool::ChunkedResponse::default();
        capture_response(
            &request,
            io_buffer,
            conn,
            &mut captured,
            pool,
            crate::types::BackendId::from_index(1),
        )
        .await?;
        Ok(captured)
    }

    #[tokio::test]
    async fn capture_and_write_streams_without_retention_after_limit() {
        let first = b"220 Article follows\r\n1234567890\r\n";
        let rest = b"tail\r\n.\r\n";
        let expected = [first.as_slice(), rest.as_slice()].concat();
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![rest.to_vec()]).await;
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(first);
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut writer = RecordingWriter::default();

        let mut classified = ClassifiedResponse::read_for_test(&mut conn, &request, io_buffer)
            .await
            .unwrap();
        let (bytes_written, retained) = classified
            .stream_for_test(&mut conn, &pool, crate::types::BackendId::from_index(1))
            .unwrap()
            .capture_and_write(&mut writer, &mut captured, first.len() - 1)
            .await
            .expect("oversized retained response should stream through");

        assert_eq!(bytes_written as usize, expected.len());
        assert!(retained.is_none());
        assert!(captured.is_empty());
        assert_eq!(writer.bytes, expected);
    }

    #[tokio::test]
    async fn capture_and_write_complete_oversized_response_is_not_retained() {
        let response = b"220 Article follows\r\n1234567890\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(response);
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut writer = RecordingWriter::default();

        let mut classified = ClassifiedResponse::read_for_test(&mut conn, &request, io_buffer)
            .await
            .unwrap();
        let (bytes_written, retained) = classified
            .stream_for_test(&mut conn, &pool, crate::types::BackendId::from_index(1))
            .unwrap()
            .capture_and_write(&mut writer, &mut captured, response.len() - 1)
            .await
            .expect("complete oversized retained response should still write");

        assert_eq!(bytes_written as usize, response.len());
        assert!(retained.is_none());
        assert!(captured.is_empty());
        assert_eq!(writer.bytes, response);
    }

    #[tokio::test]
    async fn capture_and_write_returns_framer_bound_cache_payload_end() {
        let response = b"222 0 <test@example.com>\r\nBody\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let request = crate::protocol::RequestContext::parse(b"BODY <test@example.com>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(response);
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut writer = RecordingWriter::default();

        let mut classified = ClassifiedResponse::read_for_test(&mut conn, &request, io_buffer)
            .await
            .unwrap();
        let (bytes_written, captured_response) = classified
            .stream_for_test(&mut conn, &pool, crate::types::BackendId::from_index(0))
            .unwrap()
            .capture_and_write(&mut writer, &mut captured, response.len())
            .await
            .expect("response should be retained");

        assert_eq!(bytes_written as usize, response.len());
        let cache_input = captured_response
            .expect("retained response")
            .into_cache_ingest();
        let entry = crate::cache::CachedArticle::from_unframed_ingest_for_test(
            crate::cache::CacheIngestResponse::FramedChunked(cache_input),
            crate::cache::ttl::CacheTier::new(0),
        );
        assert_eq!(entry.status_code(), crate::protocol::StatusCode::new(222));
        let rendered = entry
            .cached_response_for(crate::protocol::RequestKind::Body, "<test@example.com>")
            .expect("cached body");
        let mut output = Vec::new();
        rendered.write_to(&mut output).await.unwrap();
        assert_eq!(output, response);
    }

    #[tokio::test]
    async fn isolated_optional_chunked_capture_drains_after_limit_without_retention() {
        let first = b"220 Article follows\r\n1234567890\r\n";
        let rest = b"tail\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![rest.to_vec()]).await;
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(first);
        let mut captured = crate::pool::ChunkedResponse::default();

        let retained = IsolatedMultilineResponse::begin(&mut conn, &mut io_buffer)
            .expect("valid initial response")
            .capture_chunked_optional(&pool, &mut captured, first.len() - 1)
            .await
            .expect("oversized isolated response should drain cleanly");

        assert!(retained.is_none());
        assert!(captured.is_empty());
        assert!(!conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn isolated_cursor_rejects_a_packed_multiline_suffix() {
        let packed = b"220 Article follows\r\nbody\r\n.\r\n223 1 <next@test> exists\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(packed);

        assert!(matches!(
            IsolatedMultilineResponse::begin(&mut conn, &mut io_buffer),
            Err(FramingError::UnexpectedTrailingResponseBytes)
        ));
    }

    #[tokio::test]
    async fn isolated_optional_chunked_capture_retains_exact_limit_response() {
        let response = b"220 Article follows\r\n1234567890\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(response);
        let mut captured = crate::pool::ChunkedResponse::default();

        let retained = IsolatedMultilineResponse::begin(&mut conn, &mut io_buffer)
            .expect("valid initial response")
            .capture_chunked_optional(&pool, &mut captured, response.len())
            .await
            .expect("boundary-sized isolated response should capture cleanly");

        assert!(retained.is_some());
        assert_eq!(captured.to_vec(), response);
        assert!(!conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn isolated_optional_chunked_capture_complete_oversized_response_is_not_retained() {
        let response = b"220 Article follows\r\n1234567890\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(response);
        let mut captured = crate::pool::ChunkedResponse::default();

        let retained = IsolatedMultilineResponse::begin(&mut conn, &mut io_buffer)
            .expect("valid initial response")
            .capture_chunked_optional(&pool, &mut captured, response.len() - 1)
            .await
            .expect("complete oversized isolated response should drain cleanly");

        assert!(retained.is_none());
        assert!(captured.is_empty());
        assert!(!conn.has_pending_bytes());
    }

    async fn mock_backend_conn(chunks: Vec<Vec<u8>>) -> crate::stream::ConnectionStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for chunk in chunks {
                stream.write_all(&chunk).await.unwrap();
            }
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        crate::stream::ConnectionStream::plain(stream)
    }

    #[tokio::test]
    async fn capture_response_returns_complete_multiline_response() {
        let response = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured =
            capture_response_for_test(&mut conn, response, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn capture_response_handles_empty_multiline_body() {
        let response = b"220 0 Article follows\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured =
            capture_response_for_test(&mut conn, response, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn capture_response_preserves_queued_next_reply_from_same_read() {
        let first_response = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let next_reply = b"223 0 <next@example>\r\n";
        let mut combined = Vec::from(first_response.as_slice());
        combined.extend_from_slice(next_reply);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured =
            capture_response_for_test(&mut conn, &combined, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .unwrap();

        assert_eq!(captured.to_vec(), first_response);
        assert!(conn.has_pending_bytes());
        assert_eq!(conn.pending_bytes_len(), next_reply.len());
    }

    #[tokio::test]
    async fn capture_response_preserves_queued_next_reply_from_later_read() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n";
        let tail_chunk = b".\r\n223 0 <next@example>\r\n".to_vec();

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![tail_chunk]).await;

        let captured =
            capture_response_for_test(&mut conn, first_chunk, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .unwrap();

        assert_eq!(captured.to_vec(), b"220 Article follows\r\nLine 1\r\n.\r\n");
        assert!(conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn capture_response_handles_large_article_across_chunks() {
        let header = b"220 Article follows\r\n";
        let mut body = Vec::new();
        for i in 0..1000 {
            body.extend_from_slice(format!("Line {i}\r\n").as_bytes());
        }

        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(header);
        expected_response.extend_from_slice(&body);
        expected_response.extend_from_slice(b".\r\n");

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![expected_response[header.len()..].to_vec()]).await;

        let captured =
            capture_response_for_test(&mut conn, header, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .expect("large multiline response should capture successfully");

        assert_eq!(captured.to_vec(), expected_response);
    }

    #[tokio::test]
    async fn capture_response_moves_scratch_buffers_across_chunks() {
        let header = b"220 Article follows\r\n";
        let mut body_chunk = vec![b'x'; 94];
        body_chunk.extend_from_slice(b"\r\n");
        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(header);
        expected_response.extend_from_slice(&body_chunk);
        expected_response.extend_from_slice(b".\r\n");

        let pool = crate::pool::BufferPool::new(BufferSize::try_new(64).unwrap(), 2)
            .with_capture_pool(64, 4);
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(header);
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![body_chunk.clone(), b".\r\n".to_vec()]).await;
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");

        capture_response(
            &request,
            io_buffer,
            &mut conn,
            &mut captured,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("multiline response capture should succeed");

        assert_eq!(captured.to_vec(), expected_response);
        assert_eq!(
            pool.available_buffers(),
            0,
            "captured multiline response should hold pooled read buffers instead of copying into capture buffers"
        );
    }

    #[tokio::test]
    async fn capture_response_errors_on_full_buffer_mid_response() {
        let first_chunk = b"220 Article follows\r\nLong body content here";
        let expected_tail = b" more body\r\n.\r\n";
        let mut second_chunk = expected_tail.to_vec();
        second_chunk.resize(4096, b'X');

        let pool =
            crate::pool::BufferPool::new(crate::types::BufferSize::try_new(4096).unwrap(), 2);
        let mut conn = mock_backend_conn(vec![second_chunk]).await;

        let err =
            capture_response_for_test(&mut conn, first_chunk, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .unwrap_err();

        assert!(matches!(err, ResponseTransferError::BackendEof { .. }));
        assert!(!conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn capture_response_completes_request_context() {
        let response = b"223 0 <test@example>\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;
        let backend_id = crate::types::BackendId::from_index(1);
        let mut request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request line");

        let captured =
            capture_response_for_test(&mut conn, response, b"STAT <test@example>\r\n", &pool)
                .await
                .unwrap();
        request.complete_backend_response(
            backend_id,
            crate::protocol::StatusCode::new(223),
            captured,
        );

        assert_eq!(
            request.response_status(),
            Some(crate::protocol::StatusCode::new(223))
        );
        assert_eq!(request.backend_id(), Some(backend_id));
        assert_eq!(request.response_payload_eq(response), Some(true));
    }

    #[tokio::test]
    async fn capture_response_errors_on_truncated_multiline_response() {
        let partial = b"220 Article follows\r\nIncomplete body\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let result =
            capture_response_for_test(&mut conn, partial, b"ARTICLE <test@example>\r\n", &pool)
                .await;

        assert!(matches!(
            result,
            Err(ResponseTransferError::BackendEof { .. })
        ));
    }

    #[tokio::test]
    async fn article_430_response_is_captured_as_single_line() {
        let pool = make_pool();
        let combined = b"430 No article with that message-id\r\n223 0 <next@example>\r\n";
        let mut conn = mock_backend_conn(vec![]).await;

        let captured =
            capture_response_for_test(&mut conn, combined, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .expect("430 article miss should stay single-line");

        assert_eq!(
            captured.to_vec(),
            b"430 No article with that message-id\r\n"
        );
        assert!(conn.has_pending_bytes());
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next@example>\r\n".len());
    }

    #[tokio::test]
    async fn capture_response_handles_complete_article_with_queued_pending_bytes() {
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let result = capture_response_for_test(
            &mut conn,
            b"220 Article follows\r\nbody\r\n.\r\n223 0 <next@example>\r\n",
            b"ARTICLE <test@example>\r\n",
            &pool,
        )
        .await;

        assert!(result.is_ok());
        assert!(conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn single_line_capture_preserves_next_reply() {
        let pool = make_pool();
        let combined = b"223 0 <test@example>\r\n430 No article with that message-id\r\n";
        let mut conn = mock_backend_conn(vec![]).await;

        let captured =
            capture_response_for_test(&mut conn, combined, b"STAT <test@example>\r\n", &pool)
                .await
                .expect("single-line response should capture cleanly");

        assert_eq!(captured.to_vec(), b"223 0 <test@example>\r\n");
        assert!(conn.has_pending_bytes());
        assert_eq!(
            conn.pending_bytes_len(),
            b"430 No article with that message-id\r\n".len()
        );
    }

    #[tokio::test]
    async fn capture_response_keeps_single_line_queued_reply_out_of_pool_buffers() {
        let pool = crate::pool::BufferPool::new(crate::types::BufferSize::try_new(64).unwrap(), 2);
        assert_eq!(pool.stats(), (0, 0, 2));

        {
            let combined = b"223 0 <test@example>\r\n430 No article with that message-id\r\n";
            let mut conn = mock_backend_conn(vec![]).await;

            let captured =
                capture_response_for_test(&mut conn, combined, b"STAT <test@example>\r\n", &pool)
                    .await
                    .expect("single-line response should capture cleanly");

            assert_eq!(captured.to_vec(), b"223 0 <test@example>\r\n");
        }

        assert_eq!(pool.stats(), (2, 0, 2));
    }

    #[tokio::test]
    async fn capture_response_preserves_queued_next_reply_across_boundary() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n.";
        let second_response = b"220 Next follows\r\nLine 2\r\n.\r\n";
        let mut later_chunk = b"\r\n".to_vec();
        later_chunk.extend_from_slice(second_response);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![later_chunk]).await;

        let captured =
            capture_response_for_test(&mut conn, first_chunk, b"ARTICLE <test@example>\r\n", &pool)
                .await
                .unwrap();

        assert_eq!(captured.to_vec(), b"220 Article follows\r\nLine 1\r\n.\r\n");
        assert!(conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn complete_multiline_write_queues_packed_suffix_without_copying_bytes() {
        crate::pool::buffer::reset_hot_path_allocation_metrics();
        let chunk = b"220 article\r\nbody\r\n.\r\n223 0 <next>\r\n";
        let response_len = b"220 article\r\nbody\r\n.\r\n".len();
        let framed = CompleteResponseWindow {
            response: 0..response_len,
            capture: 0..response_len,
            next_response_input: response_len..chunk.len(),
        };
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(chunk);
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        let written = framed
            .write_from(&mut writer, &mut io_buffer, &mut conn, &pool)
            .await
            .expect("complete response should write");

        assert_eq!(written.bytes_written_u64(), response_len as u64);
        assert_eq!(writer, &chunk[..response_len]);
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
        let metrics = crate::pool::buffer::hot_path_allocation_metrics_snapshot();
        assert_eq!(metrics.pending_backend_byte_heap_fallbacks, 0);
    }

    #[tokio::test]
    async fn single_line_write_queues_packed_suffix_without_copying_bytes() {
        crate::pool::buffer::reset_hot_path_allocation_metrics();
        let chunk = b"223 0 <first>\r\n223 0 <next>\r\n";
        let response_len = b"223 0 <first>\r\n".len();
        let request =
            crate::protocol::RequestContext::parse(b"STAT <first>\r\n").expect("valid request");
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(chunk);
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        let written = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("single-line response should write");

        assert_eq!(written, response_len as u64);
        assert_eq!(writer, &chunk[..response_len]);
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
        let metrics = crate::pool::buffer::hot_path_allocation_metrics_snapshot();
        assert_eq!(metrics.pending_backend_byte_heap_fallbacks, 0);
    }

    #[tokio::test]
    async fn complete_multiline_write_client_error_preserves_packed_suffix() {
        let chunk = b"220 article\r\nbody\r\n.\r\n223 0 <next>\r\n";
        let response_len = b"220 article\r\nbody\r\n.\r\n".len();
        let framed = CompleteResponseWindow {
            response: 0..response_len,
            capture: 0..response_len,
            next_response_input: response_len..chunk.len(),
        };
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(chunk);
        let mut conn = loopback_connection_stream().await;
        let mut writer = FailingWriter;

        let err = framed
            .write_from(&mut writer, &mut io_buffer, &mut conn, &pool)
            .await;

        assert!(matches!(
            err,
            Err(ResponseTransferError::ClientDisconnect(_))
        ));
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
    }

    #[tokio::test]
    async fn single_line_write_client_error_preserves_packed_suffix() {
        let chunk = b"223 0 <first>\r\n223 0 <next>\r\n";
        let request =
            crate::protocol::RequestContext::parse(b"STAT <first>\r\n").expect("valid request");
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(chunk);
        let mut conn = loopback_connection_stream().await;
        let mut writer = FailingWriter;

        let err = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await;

        assert!(matches!(
            err,
            Err(ResponseTransferError::ClientDisconnect(_))
        ));
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
    }

    #[tokio::test]
    async fn complete_multiline_split_boundaries_preserve_packed_suffix_on_write_error() {
        let first_response = b"220 article\r\nbody\r\n.\r\n";
        let next_response = b"223 0 <next>\r\n";
        let pool = make_pool();

        for split in 1..first_response.len() {
            let initial = &first_response[..split];
            let mut continuation = Vec::from(&first_response[split..]);
            continuation.extend_from_slice(next_response);
            let mut framer = MultilineFramer::default();
            match framer.frame_multiline_chunk(initial) {
                ResponseWindow::Incomplete(_) => {}
                ResponseWindow::Complete(_) => panic!("split={split} unexpectedly complete"),
            };
            let complete = match framer.frame_multiline_chunk(&continuation) {
                ResponseWindow::Complete(complete) => complete,
                ResponseWindow::Incomplete(_) => {
                    panic!("split={split} did not complete on continuation")
                }
            };

            let mut io_buffer = pool.acquire();
            io_buffer.copy_from_slice(&continuation);
            let mut conn = loopback_connection_stream().await;
            let mut writer = FailingWriter;

            let err = complete
                .write_from(&mut writer, &mut io_buffer, &mut conn, &pool)
                .await;

            assert!(matches!(
                err,
                Err(ResponseTransferError::ClientDisconnect(_))
            ));
            assert_eq!(
                conn.pending_bytes_len(),
                next_response.len(),
                "split={split}"
            );
        }
    }

    #[tokio::test]
    async fn write_response_complete_multiline_queues_packed_suffix_as_pooled_input() {
        let first_response = b"220 article\r\nbody\r\n.\r\n";
        let next_response = b"223 0 <next>\r\n";
        let mut chunk = Vec::from(first_response.as_slice());
        chunk.extend_from_slice(next_response);

        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(&chunk);
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        let written = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("complete multiline response should write");

        assert_eq!(written, first_response.len() as u64);
        assert_eq!(writer, first_response);
        assert_eq!(conn.pending_bytes_len(), next_response.len());
        assert_eq!(
            pool.available_buffers(),
            1,
            "the consumed response returns its replacement scratch buffer; the packed suffix retains the original"
        );

        let mut pending = vec![0; next_response.len()];
        conn.read_exact(&mut pending).await.unwrap();
        assert_eq!(pending, next_response);
        assert_eq!(
            pool.available_buffers(),
            2,
            "draining pooled pending input also returns the original buffer"
        );
    }

    #[tokio::test]
    async fn ordered_response_reuses_packed_suffix_buffer_without_copying() {
        let first_response = b"220 article\r\nbody\r\n.\r\n";
        let next_response = b"223 0 <next>\r\n";
        let mut chunk = Vec::from(first_response.as_slice());
        chunk.extend_from_slice(next_response);

        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(&chunk);
        let packed_buffer_ptr = io_buffer.allocation_ptr();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("complete multiline response should write");

        let next_buffer = take_queued_input_or_acquire_empty(&mut conn, &pool);

        assert_eq!(next_buffer.allocation_ptr(), packed_buffer_ptr);
        assert_eq!(next_buffer.as_ref(), next_response);
        assert!(!conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn repeated_packed_responses_keep_reusing_the_original_buffer() {
        let first_response = b"220 first\r\nfirst body\r\n.\r\n";
        let second_response = b"220 second\r\nsecond body\r\n.\r\n";
        let third_response = b"223 0 <third>\r\n";
        let mut chunk = Vec::from(first_response.as_slice());
        chunk.extend_from_slice(second_response);
        chunk.extend_from_slice(third_response);

        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(&chunk);
        let original_allocation = io_buffer.allocation_ptr();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("first response should write");

        let second_buffer = take_queued_input_or_acquire_empty(&mut conn, &pool);
        assert_eq!(second_buffer.allocation_ptr(), original_allocation);
        assert_eq!(second_buffer.as_ref(), &chunk[first_response.len()..]);
        writer.clear();
        write_response(
            &request,
            second_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("second response should write");
        assert_eq!(writer, second_response);

        let third_buffer = take_queued_input_or_acquire_empty(&mut conn, &pool);
        assert_eq!(third_buffer.allocation_ptr(), original_allocation);
        assert_eq!(third_buffer.as_ref(), third_response);
        assert!(!conn.has_pending_bytes());
    }

    #[tokio::test]
    async fn packed_multiline_prefix_and_completion_use_one_contiguous_write_at_every_split() {
        let response = b"220 article\r\nbody\r\n.\r\n";
        let next_response = b"223 0 <next>\r\n";
        let status_line_len = b"220 article\r\n".len();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");

        for split in status_line_len..response.len() {
            let pool = make_pool();
            let mut packed = b"discard".to_vec();
            packed.extend_from_slice(&response[..split]);
            let mut io_buffer = pool.acquire();
            io_buffer.copy_from_slice(&packed);
            io_buffer.expose_initialized_range_without_copying(7..packed.len());
            let mut continuation = response[split..].to_vec();
            continuation.extend_from_slice(next_response);
            let mut conn = mock_backend_conn(vec![continuation]).await;
            let mut writer = RecordingWriter::default();

            let written = write_response(
                &request,
                io_buffer,
                &mut conn,
                &mut writer,
                &pool,
                crate::types::BackendId::from_index(1),
            )
            .await
            .unwrap_or_else(|error| panic!("split={split}: {error}"));

            assert_eq!(written, response.len() as u64, "split={split}");
            assert_eq!(writer.bytes, response, "split={split}");
            assert_eq!(writer.scalar_writes, 1, "split={split}");
            assert_eq!(writer.vectored_writes, 0, "split={split}");
            let mut pending = vec![0; next_response.len()];
            conn.read_exact(&mut pending).await.unwrap();
            assert_eq!(pending, next_response, "split={split}");
        }
    }

    #[tokio::test]
    async fn packed_contiguous_write_failure_preserves_the_next_response_at_every_split() {
        let response = b"220 article\r\nbody\r\n.\r\n";
        let next_response = b"223 0 <next>\r\n";
        let status_line_len = b"220 article\r\n".len();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");

        for split in status_line_len..response.len() {
            let pool = make_pool();
            let mut packed = b"discard".to_vec();
            packed.extend_from_slice(&response[..split]);
            let mut io_buffer = pool.acquire();
            io_buffer.copy_from_slice(&packed);
            io_buffer.expose_initialized_range_without_copying(7..packed.len());
            let mut continuation = response[split..].to_vec();
            continuation.extend_from_slice(next_response);
            let mut conn = mock_backend_conn(vec![continuation]).await;
            let mut writer = FailingWriter;

            let error = write_response(
                &request,
                io_buffer,
                &mut conn,
                &mut writer,
                &pool,
                crate::types::BackendId::from_index(1),
            )
            .await;

            assert!(
                matches!(error, Err(ResponseTransferError::ClientDisconnect(_))),
                "split={split}"
            );
            let mut pending = vec![0; next_response.len()];
            conn.read_exact(&mut pending).await.unwrap();
            assert_eq!(pending, next_response, "split={split}");
        }
    }

    #[tokio::test]
    async fn ordinary_incomplete_multiline_response_keeps_streaming_scalar_writes() {
        let first = b"220 article\r\nbody";
        let rest = b"\r\n.\r\n";
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(first);
        let mut conn = mock_backend_conn(vec![rest.to_vec()]).await;
        let mut writer = RecordingWriter::default();

        write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("ordinary streaming response should write");

        assert_eq!(writer.bytes, [first.as_slice(), rest.as_slice()].concat());
        assert_eq!(writer.scalar_writes, 2);
        assert_eq!(writer.vectored_writes, 0);
    }

    #[tokio::test]
    async fn write_response_single_line_queues_packed_suffix_as_pooled_input() {
        let first_response = b"223 0 <first>\r\n";
        let next_response = b"223 0 <next>\r\n";
        let mut chunk = Vec::from(first_response.as_slice());
        chunk.extend_from_slice(next_response);

        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(&chunk);
        let request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request");
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        let written = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("single-line response should write");

        assert_eq!(written, first_response.len() as u64);
        assert_eq!(writer, first_response);
        assert_eq!(conn.pending_bytes_len(), next_response.len());
        assert_eq!(
            pool.available_buffers(),
            1,
            "the consumed response returns its replacement scratch buffer; the packed suffix retains the original"
        );

        let mut pending = vec![0; next_response.len()];
        conn.read_exact(&mut pending).await.unwrap();
        assert_eq!(pending, next_response);
        assert_eq!(
            pool.available_buffers(),
            2,
            "draining pooled pending input also returns the original buffer"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn write_response_single_line_records_response_write_metrics() {
        let _guard = crate::pool::buffer::response_write_metrics_test_guard();
        crate::pool::buffer::reset_response_write_metrics();
        crate::pool::buffer::set_response_write_metrics_enabled(true);

        let response = b"223 0 <first>\r\n";
        let request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request");
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(response);
        let mut conn = loopback_connection_stream().await;
        let mut writer = Vec::new();

        let written = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("single-line response should write");

        let metrics = crate::pool::buffer::response_write_metrics_snapshot();
        assert_eq!(written, response.len() as u64);
        assert_eq!(metrics.responses, 1);
        assert_eq!(metrics.single_chunk_responses, 1);
        assert_eq!(metrics.multi_chunk_responses, 0);
        assert_eq!(metrics.chunks_written, 1);
        assert_eq!(metrics.bytes_written, response.len());
        assert_eq!(metrics.tiny_chunks, 1);
        assert_eq!(metrics.tiny_chunk_bytes, response.len());
        assert_eq!(metrics.small_chunks, 1);
        assert_eq!(metrics.small_chunk_bytes, response.len());
        assert_eq!(metrics.max_chunks_per_response, 1);

        crate::pool::buffer::set_response_write_metrics_enabled(false);
        crate::pool::buffer::reset_response_write_metrics();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn write_response_multiline_records_multi_chunk_response_write_metrics() {
        let _guard = crate::pool::buffer::response_write_metrics_test_guard();
        crate::pool::buffer::reset_response_write_metrics();
        crate::pool::buffer::set_response_write_metrics_enabled(true);

        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(b"220 article\r\n");
        let mut conn = mock_backend_conn(vec![b"body\r\n.\r\n".to_vec()]).await;
        let mut writer = Vec::new();

        let written = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await
        .expect("multiline response should write");

        let metrics = crate::pool::buffer::response_write_metrics_snapshot();
        let expected = b"220 article\r\nbody\r\n.\r\n";
        assert_eq!(written, expected.len() as u64);
        assert_eq!(writer, expected);
        assert_eq!(metrics.responses, 1);
        assert_eq!(metrics.single_chunk_responses, 0);
        assert_eq!(metrics.multi_chunk_responses, 1);
        assert_eq!(metrics.chunks_written, 2);
        assert_eq!(metrics.bytes_written, expected.len());
        assert_eq!(metrics.tiny_chunks, 2);
        assert_eq!(metrics.tiny_chunk_bytes, expected.len());
        assert_eq!(metrics.small_chunks, 2);
        assert_eq!(metrics.small_chunk_bytes, expected.len());
        assert_eq!(metrics.max_chunks_per_response, 2);

        crate::pool::buffer::set_response_write_metrics_enabled(false);
        crate::pool::buffer::reset_response_write_metrics();
    }

    #[tokio::test]
    async fn incomplete_multiline_client_error_consumes_backend_response_boundary() {
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(b"220 article\r\n");
        let mut conn = mock_backend_conn(vec![b"body\r\n.\r\n223 0 <next>\r\n".to_vec()]).await;
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut writer = FailingWriter;

        let err = write_response(
            &request,
            io_buffer,
            &mut conn,
            &mut writer,
            &pool,
            crate::types::BackendId::from_index(1),
        )
        .await;

        assert!(matches!(
            err,
            Err(ResponseTransferError::ClientDisconnect(_))
        ));
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
    }

    #[test]
    fn backend_response_order_releases_deferred_reply_after_tracked_reply() {
        let request = crate::protocol::RequestContext::parse(b"DATE\r\n").expect("valid request");
        let mut order = BackendResponseOrder::default();
        order.push_request(request.kind());
        order.push_deferred_reply(b"205 Goodbye\r\n");

        assert!(order.has_pending_backend_replies());
        assert!(order.has_deferred_replies());
        assert!(order.should_drain_backend_replies());

        let writes = order.client_writes_for_backend_read(b"111 20260520120000\r\n");

        assert_eq!(writes.len(), 2);
        assert_eq!(&writes[0][..], b"111 20260520120000\r\n");
        assert_eq!(&writes[1][..], b"205 Goodbye\r\n");
        assert!(!order.has_pending_backend_replies());
        assert!(!order.has_deferred_replies());
    }

    #[test]
    fn xhdr_221_response_completes_when_fragmented_before_deferred_reply() {
        let request = crate::protocol::RequestContext::parse(b"XHDR Subject 1-10\r\n")
            .expect("valid request");
        let mut order = BackendResponseOrder::default();
        order.push_request(request.kind());
        order.push_deferred_reply(b"205 Goodbye\r\n");

        let first = order.client_writes_for_backend_read(b"221 1 Subject\r\nvalue\r\n");
        assert_eq!(first.len(), 1);
        assert_eq!(&first[0][..], b"221 1 Subject\r\nvalue\r\n");

        let second = order.client_writes_for_backend_read(b".\r\n");
        assert_eq!(second.len(), 2);
        assert_eq!(&second[0][..], b".\r\n");
        assert_eq!(&second[1][..], b"205 Goodbye\r\n");
        assert!(!order.has_pending_backend_replies());
        assert!(!order.has_deferred_replies());
    }

    #[test]
    fn backend_response_order_keeps_common_writes_inline_and_borrowed() {
        let request = crate::protocol::RequestContext::parse(b"DATE\r\n").expect("valid request");
        let mut order = BackendResponseOrder::default();
        order.push_request(request.kind());
        order.push_deferred_reply(b"205 Goodbye\r\n");

        let writes = order.client_writes_for_backend_read(b"111 20260520120000\r\n");

        assert!(
            !writes.spilled(),
            "normal ordered backend reads should not allocate a heap writes vec"
        );
        assert!(matches!(writes[0], std::borrow::Cow::Borrowed(_)));
        assert!(matches!(writes[1], std::borrow::Cow::Borrowed(_)));
        assert_eq!(&writes[0][..], b"111 20260520120000\r\n");
        assert_eq!(&writes[1][..], b"205 Goodbye\r\n");
    }

    #[test]
    fn backend_response_order_splits_packed_multiline_and_single_line_replies() {
        let help = crate::protocol::RequestContext::parse(b"HELP\r\n").expect("valid request");
        let date = crate::protocol::RequestContext::parse(b"DATE\r\n").expect("valid request");
        let mut order = BackendResponseOrder::default();
        order.push_request(help.kind());
        order.push_request(date.kind());

        let backend_read = b"100 Help follows\r\nfirst line\r\n.\r\n111 20260520120000\r\n";
        let writes = order.client_writes_for_backend_read(backend_read);

        assert_eq!(writes.len(), 2);
        assert_eq!(&writes[0][..], b"100 Help follows\r\nfirst line\r\n.\r\n");
        assert_eq!(&writes[1][..], b"111 20260520120000\r\n");
        assert!(!order.has_pending_backend_replies());
    }

    fn for_each_critical_byte_sequence(max_len: usize, f: &mut impl FnMut(&[u8])) {
        let mut data = Vec::with_capacity(max_len);
        f(&data);

        fn recurse(data: &mut Vec<u8>, max_len: usize, f: &mut impl FnMut(&[u8])) {
            if data.len() == max_len {
                return;
            }

            for byte in EXHAUSTIVE_BYTES {
                data.push(byte);
                f(data);
                recurse(data, max_len, f);
                data.pop();
            }
        }

        recurse(&mut data, max_len, f);
    }

    fn run_streaming_detection(data: &[u8], boundary_mask: u32) -> Option<usize> {
        if data.is_empty() {
            return None;
        }

        let mut framer = MultilineFramer::default();
        let mut chunk_start = 0usize;

        for idx in 0..data.len() {
            let is_last = idx + 1 == data.len();
            let split_after = !is_last && ((boundary_mask >> idx) & 1) == 1;
            if !is_last && !split_after {
                continue;
            }

            let chunk_end = idx + 1;
            if let Some(pos) = framer.advance_to_next_terminator_end(&data[chunk_start..chunk_end])
            {
                return Some(chunk_start + pos);
            }
            chunk_start = chunk_end;
        }

        None
    }

    #[test]
    fn production_callers_do_not_reintroduce_raw_boundary_scanners() {
        const FORBIDDEN: &[&str] = &[
            "find_terminator",
            "next_terminator",
            "advance_to_next",
            "complete_multiline_payload",
            "payload_without_multiline",
            "chunked_multiline_payload",
            "MultilineChunkSplit",
            "CompleteMultiline",
            "PackedPendingBytesPolicy",
            "TERMINATOR_TAIL_SIZE",
            "terminator_ends",
            "write_multiline_from",
            "extend_isolated_multiline_capture",
            "observe_isolated_multiline_chunk",
            "push_isolated_multiline_buffer",
            "push_multiline_from_buffer",
            "push_multiline_from_shared",
            "MultilineFramer",
            "PendingRequestFramer",
            "FramedBackendBytes",
            "frame_backend_bytes",
            "observe_backend_bytes",
            "response_framer",
            "cached_response_end",
            "framed_payload_body",
            "chunked_framed_payload_end",
            "cached_payload_completion",
            "cache_payload_body",
            "cached_response_completion_len",
            "cache_payload_body_len",
            "parse_payload_chunked_response",
            "from_chunked_ingest_with_tier",
            "chunked_status_line_end",
            "chunked_cache_payload_body_end",
            "payload_for_chunked_status",
            "find_sequence_in_chunked_range",
            "copy_chunked_range",
            "PrefetchedMultiline",
            "PrefetchedResponse",
            "from_prefetched",
            "response_buffer",
            "response_transfer::CAPABILITIES",
            "response_transfer::cached_response_completion",
            "response_transfer::complete_response_body",
            "single_line_response_bytes",
            "complete_single_line_response_bytes",
            "backend_response_bytes",
            "ResponseCapture::",
            "IsolatedMultilineResponse",
            "BackendReplyTracker",
            "BackendReplyBytes",
            "OrderedResponse",
            "capture_prefetched_multiline_source",
            "stash_leftover",
            "pop_leftover",
            "push_front_leftover",
            "has_leftover",
            "leftover_len",
            "clear_leftover",
            "packed_suffix",
            "PackedSuffix",
            "suffix_range",
            "record_packed_suffix",
            "read_ahead",
            "ReadAhead",
            "stash_read_ahead",
            "pop_read_ahead",
            "push_front_read_ahead",
            "record_read_ahead",
        ];

        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let framing_file = src_dir.join("session").join("multiline_framing.rs");
        let mut stack = vec![src_dir];
        let mut violations = Vec::new();

        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src directory") {
                let entry = entry.expect("read src entry");
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path == framing_file
                    || path.extension().and_then(|ext| ext.to_str()) != Some("rs")
                {
                    continue;
                }

                let content = std::fs::read_to_string(&path).expect("read Rust source");
                for forbidden in FORBIDDEN {
                    if content.contains(forbidden) {
                        violations.push(format!("{} contains {forbidden}", path.display()));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "raw multiline boundary scanner shapes escaped multiline_framing.rs:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn update_preserves_rolling_tail() {
        let mut framer = MultilineFramer::default();
        framer.update(b"\r\n");
        assert_eq!(&framer.data[..framer.len], b"\r\n");

        framer.update(b".");
        assert_eq!(&framer.data[..framer.len], b"\r\n.");

        framer.update(b"\r\n");
        assert_eq!(&framer.data[..framer.len], b"\n.\r\n");
        assert_eq!(framer.len, TERMINATOR_TAIL_SIZE);
    }

    #[test]
    fn update_ignores_empty_chunks() {
        let mut framer = MultilineFramer::default();
        framer.update(b"initial");
        let snapshot = framer.data[..framer.len].to_vec();
        framer.update(b"");
        assert_eq!(&framer.data[..framer.len], snapshot);
    }

    #[test]
    fn spanning_offsets_cover_all_split_points() {
        let splits = [
            (b"\r".as_slice(), b"\n.\r\n".as_slice(), 4),
            (b"\r\n".as_slice(), b".\r\n".as_slice(), 3),
            (b"\r\n.".as_slice(), b"\r\n".as_slice(), 2),
            (b"\r\n.\r".as_slice(), b"\n".as_slice(), 1),
        ];

        for (tail, chunk, expected) in splits {
            let mut framer = MultilineFramer::default();
            framer.update(tail);
            assert_eq!(framer.find_spanning_terminator(chunk), Some(expected));
            assert_eq!(framer.next_terminator_end(chunk), Some(expected));
        }
    }

    #[test]
    fn spanning_offset_handles_large_chunk() {
        let mut framer = MultilineFramer::default();
        framer.update(b"text\r\n");

        let mut chunk = b".\r\n".to_vec();
        chunk.extend(vec![b'X'; 8000]);

        assert_eq!(framer.next_terminator_end(&chunk), Some(3));
    }

    #[test]
    fn advance_to_next_terminator_end_updates_only_on_miss() {
        let mut framer = MultilineFramer::default();
        assert_eq!(
            framer.advance_to_next_terminator_end(b"body line\r\n"),
            None
        );
        assert_eq!(&framer.data[..framer.len], b"ne\r\n");

        let snapshot = framer.data[..framer.len].to_vec();
        assert_eq!(framer.advance_to_next_terminator_end(b".\r\n"), Some(3));
        assert_eq!(&framer.data[..framer.len], snapshot);
    }

    #[test]
    fn split_chunk_returns_complete_response_without_suffix() {
        let mut framer = MultilineFramer::default();

        let split = framer.split_chunk(
            b"220 article\r\nbody\r\n.\r\n",
            PackedPendingBytesPolicy::Reject,
        );

        assert_eq!(
            split,
            Ok(ChunkProgress::Complete(CompleteChunk {
                consumed: ChunkConsumed(b"220 article\r\nbody\r\n.\r\n".len()),
                payload_end: ChunkPayloadEnd(b"220 article\r\nbody".len()),
            }))
        );
    }

    #[test]
    fn split_chunk_returns_complete_response_with_allowed_suffix() {
        let mut framer = MultilineFramer::default();
        let chunk = b"220 article\r\nbody\r\n.\r\n223 0 <next>\r\n";

        let split = framer.split_chunk(chunk, PackedPendingBytesPolicy::AllowIfStatusPrefix);

        assert_eq!(
            split,
            Ok(ChunkProgress::Complete(CompleteChunk {
                consumed: ChunkConsumed(b"220 article\r\nbody\r\n.\r\n".len()),
                payload_end: ChunkPayloadEnd(b"220 article\r\nbody".len()),
            }))
        );
    }

    #[test]
    fn split_chunk_rejects_packed_pending_bytes_when_forbidden() {
        let mut framer = MultilineFramer::default();
        let chunk = b"220 article\r\nbody\r\n.\r\n223 0 <next>\r\n";

        let split = framer.split_chunk(chunk, PackedPendingBytesPolicy::Reject);

        assert_eq!(split, Err(FramingError::UnexpectedTrailingResponseBytes));
    }

    #[test]
    fn split_chunk_treats_invalid_suffix_terminator_as_payload() {
        let mut framer = MultilineFramer::default();
        let chunk = b"220 article\r\npayload\r\n.\r\nnot-a-status\r\n.\r\n";

        let split = framer.split_chunk(chunk, PackedPendingBytesPolicy::AllowIfStatusPrefix);

        assert_eq!(
            split,
            Ok(ChunkProgress::Complete(CompleteChunk {
                consumed: ChunkConsumed(chunk.len()),
                payload_end: ChunkPayloadEnd(chunk.len() - TERMINATOR.len()),
            }))
        );
    }

    #[test]
    fn split_chunk_returns_complete_response_for_spanning_terminator() {
        let mut framer = MultilineFramer::default();
        let _ = framer.split_chunk(b"220 article\r\nbody\r\n", PackedPendingBytesPolicy::Reject);

        let split = framer.split_chunk(b".\r\n", PackedPendingBytesPolicy::Reject);

        assert_eq!(
            split,
            Ok(ChunkProgress::Complete(CompleteChunk {
                consumed: ChunkConsumed(b".\r\n".len()),
                payload_end: ChunkPayloadEnd(0),
            }))
        );
    }

    #[test]
    fn incremental_framer_matches_rescan_for_every_two_push_split() {
        let response = b"220 article\r\nbody line\r\n.\r\n";
        let expected = response.len();

        for split in 0..=response.len() {
            let mut framer = MultilineFramer::default();
            let first = framer
                .split_chunk(
                    &response[..split],
                    PackedPendingBytesPolicy::AllowIfStatusPrefix,
                )
                .expect("first push should not reject a valid response");
            let actual = match first {
                ChunkProgress::Complete(complete) => Some(complete.consumed.0),
                ChunkProgress::Incomplete => framer
                    .split_chunk(
                        &response[split..],
                        PackedPendingBytesPolicy::AllowIfStatusPrefix,
                    )
                    .ok()
                    .and_then(|result| match result {
                        ChunkProgress::Complete(complete) => Some(split + complete.consumed.0),
                        ChunkProgress::Incomplete => None,
                    }),
            };

            assert_eq!(actual, Some(expected), "split={split}");

            let mut stateless = MultilineFramer::default();
            let rescanned_end = stateless
                .split_chunk(response, PackedPendingBytesPolicy::AllowIfStatusPrefix)
                .expect("rescan should accept a valid response");
            let ChunkProgress::Complete(rescanned) = rescanned_end else {
                panic!("rescan did not complete for split={split}");
            };
            assert_eq!(actual, Some(rescanned.consumed.0), "split={split}");
        }
    }

    #[test]
    fn backend_reply_tracker_consumes_invalid_status_line() {
        let mut tracker = BackendReplyTracker::default();
        tracker.push_request(crate::protocol::RequestKind::Article);

        let output = tracker.accept_backend_bytes(b"not-a-status\r\n");

        assert_eq!(
            output.as_slice(),
            &[BackendReplyBytes::CompletedTrackedReply(
                b"not-a-status\r\n"
            )]
        );
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn complete_multiline_payload_body_returns_content_without_dot_terminator() {
        assert_eq!(complete_multiline_payload_body(b".\r\n"), Some(&b""[..]));
        assert_eq!(
            complete_multiline_payload_body(b"body\r\n.\r\n"),
            Some(&b"body\r\n"[..])
        );
        assert_eq!(complete_multiline_payload_body(b"body\r\n.\r\nextra"), None);
    }

    #[test]
    fn complete_multiline_payload_split_returns_body_and_terminator_ranges() {
        let payload = b"body\r\n.\r\n";

        let split = complete_multiline_payload_split(payload).expect("complete payload");

        assert_eq!(&payload[split.body()], b"body\r\n");
        assert_eq!(&payload[split.terminator()], b".\r\n");
    }

    #[test]
    fn find_terminator_end_finds_first_complete_terminator() {
        let data = b"222 1 <a@b>\r\nbody-1\r\n.\r\n222 2 <c@d>\r\nbody-2\r\n.\r\n";
        let end = find_terminator_end(data).expect("first terminator should be found");

        assert_eq!(end, b"222 1 <a@b>\r\nbody-1\r\n.\r\n".len());
        assert_eq!(&data[end - 5..end], TERMINATOR);
        assert!(end < data.len());
    }

    #[test]
    fn find_terminator_end_must_not_prefer_buffer_suffix() {
        const BUFFER_LEN: usize = 4096;
        let first_terminator_start = (BUFFER_LEN - TERMINATOR.len()) / 2;
        let suffix_terminator_start = BUFFER_LEN - TERMINATOR.len();
        let mut data = vec![b'x'; BUFFER_LEN];

        data[first_terminator_start..first_terminator_start + TERMINATOR.len()]
            .copy_from_slice(TERMINATOR);
        data[suffix_terminator_start..].copy_from_slice(TERMINATOR);

        assert_eq!(
            find_terminator_end(&data),
            Some(first_terminator_start + TERMINATOR.len()),
            "terminator detection must scan for the first terminator, not only check the end of the read buffer"
        );
        assert_ne!(
            find_terminator_end(&data),
            Some(BUFFER_LEN),
            "a suffix-only check would incorrectly return the terminator at the end of the buffer"
        );
    }

    #[test]
    fn find_terminator_end_rejects_false_positives() {
        assert_eq!(find_terminator_end(b""), None);
        assert_eq!(find_terminator_end(b"line\r\n."), None);
        assert_eq!(find_terminator_end(b"data with \r\n but no dot"), None);
    }

    #[test]
    fn exhaustive_single_chunk_detection_matches_reference() {
        for_each_critical_byte_sequence(8, &mut |data| {
            let expected =
                memchr::memmem::find(data, TERMINATOR).map(|start| start + TERMINATOR.len());
            assert_eq!(
                find_terminator_end(data),
                expected,
                "single-chunk mismatch for {:?}",
                data
            );
        });
    }

    #[test]
    fn exhaustive_streaming_detection_matches_reference_for_all_chunkings() {
        for_each_critical_byte_sequence(7, &mut |data| {
            let expected =
                memchr::memmem::find(data, TERMINATOR).map(|start| start + TERMINATOR.len());
            let boundary_variants = 1u32 << data.len().saturating_sub(1);

            for boundary_mask in 0..boundary_variants {
                assert_eq!(
                    run_streaming_detection(data, boundary_mask),
                    expected,
                    "streaming mismatch for {:?} with boundary mask {:b}",
                    data,
                    boundary_mask
                );
            }
        });
    }
}

#[cfg(response_contract)]
#[allow(dead_code)]
mod contracts {
    use super::*;

    fn chunk_coordinate() {
        let origin = WindowOffset::new(12);
        let consumed = ChunkConsumed(3);
        #[cfg(response_contract = "chunk_coordinate")]
        let consumed = crate::protocol::FrameEnd::new(consumed.0);
        std::hint::black_box(origin.after_chunk(consumed));
    }

    async fn classified_buffer_reuse(
        request: &crate::protocol::RequestContext,
        buffer: crate::pool::PooledBuffer,
        reader: &mut (impl tokio::io::AsyncRead + Unpin),
    ) {
        let response = ClassifiedResponse::read_for_test(reader, request, buffer)
            .await
            .expect("classified response");
        #[cfg(response_contract = "classified_buffer_reuse")]
        let _replaced = buffer;
        std::hint::black_box(response.single_line_bytes());
    }

    async fn consuming_response(
        request: &crate::protocol::RequestContext,
        buffer: crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        pool: &crate::pool::BufferPool,
        writer: &mut (impl AsyncWrite + Unpin),
    ) {
        let mut response = ClassifiedResponse::read_for_test(conn, request, buffer)
            .await
            .expect("classified response");
        let mut streaming = response
            .stream_for_test(conn, pool, crate::types::BackendId::from_index(0))
            .expect("classified response should stream");
        let transfer = streaming.write(writer);
        #[cfg(response_contract = "response_twice")]
        let _conflicting = response.status_code();
        std::hint::black_box(transfer);
    }
}
