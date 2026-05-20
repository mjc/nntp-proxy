//! Multiline response framing for tracking terminators across chunk boundaries
//!
//! Used to detect NNTP terminators that span across chunk boundaries.

use std::collections::VecDeque;
use std::ops::Range;

use anyhow::Context;
use tokio::io::{AsyncWrite, AsyncWriteExt};

const TERMINATOR: &[u8; 5] = b"\r\n.\r\n";
const TERMINATOR_TAIL_SIZE: usize = 4;

#[must_use]
pub(crate) fn cached_response_completion() -> std::io::IoSlice<'static> {
    std::io::IoSlice::new(TERMINATOR)
}

pub(crate) const CAPABILITIES_WITHOUT_AUTHINFO_RESPONSE: &[u8] =
    b"101 Capability list:\r\nVERSION 2\r\nREADER\r\nOVER\r\nHDR\r\n.\r\n";

pub(crate) const CAPABILITIES_WITH_AUTHINFO_RESPONSE: &[u8] =
    b"101 Capability list:\r\nVERSION 2\r\nREADER\r\nAUTHINFO USER PASS\r\nOVER\r\nHDR\r\n.\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackedPendingBytesPolicy {
    Reject,
    AllowIfStatusPrefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompleteMultilinePayloadSplit {
    body: Range<usize>,
    terminator: Range<usize>,
}

impl CompleteMultilinePayloadSplit {
    #[must_use]
    pub(crate) const fn new(body: Range<usize>, terminator: Range<usize>) -> Self {
        Self { body, terminator }
    }

    #[must_use]
    pub(crate) fn body(&self) -> Range<usize> {
        self.body.clone()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn terminator(&self) -> Range<usize> {
        self.terminator.clone()
    }
}

/// Helper for tracking the last few bytes of streamed data
///
/// Used to detect terminators that span across chunk boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompleteMultilineWireChunk {
    response: Range<usize>,
    next_response_input: Range<usize>,
}

impl CompleteMultilineWireChunk {
    pub(crate) fn extend_capture_from(
        &self,
        source: &[u8],
        capture: &mut crate::pool::PooledBuffer,
    ) {
        capture.extend_from_slice(&source[self.response.clone()]);
    }

    pub(crate) fn push_isolated_buffer_to(
        &self,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
        buffer: &mut crate::pool::PooledBuffer,
    ) {
        let old = std::mem::replace(buffer, pool.acquire());
        response.push_buffer_range(old, self.response.clone());
    }

    pub(crate) async fn write_from<W>(
        &self,
        writer: &mut W,
        buffer: &[u8],
        conn: &mut crate::stream::ConnectionStream,
    ) -> Result<u64, crate::session::response_buffer::ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let response = &buffer[self.response.clone()];
        if self.next_response_input.start < buffer.len() {
            conn.queue_pending_bytes_first(&buffer[self.next_response_input.clone()])
                .map_err(crate::session::response_buffer::ResponseTransferError::Io)?;
        }
        writer
            .write_all(response)
            .await
            .map_err(crate::session::response_buffer::ResponseTransferError::ClientDisconnect)?;
        crate::pool::buffer::record_non_owned_response_write_chunk(response.len());
        Ok(response.len() as u64)
    }

    pub(crate) fn push_from_buffer(
        &self,
        io_buffer: &mut crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
        total_len: usize,
    ) -> Result<(), crate::session::response_buffer::ResponseTransferError> {
        let old = std::mem::replace(io_buffer, pool.acquire());
        if self.next_response_input.start < total_len {
            conn.queue_pending_bytes_first(&old[self.next_response_input.clone()])
                .map_err(crate::session::response_buffer::ResponseTransferError::Io)?;
        }
        response.push_buffer_range(old, self.response.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncompleteMultilineWireChunk {
    response: Range<usize>,
}

impl IncompleteMultilineWireChunk {
    async fn write_from<W>(
        &self,
        writer: &mut W,
        source: &[u8],
    ) -> Result<u64, crate::session::response_buffer::ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let response = &source[self.response.clone()];
        writer.write_all(response).await.map_err(
            crate::session::response_buffer::ResponseTransferError::ClientDisconnectBeforeResponseComplete,
        )?;
        crate::pool::buffer::record_non_owned_response_write_chunk(response.len());
        Ok(response.len() as u64)
    }

    pub(crate) fn extend_capture_from(
        &self,
        source: &[u8],
        capture: &mut crate::pool::PooledBuffer,
    ) {
        capture.extend_from_slice(&source[self.response.clone()]);
    }

    pub(crate) fn push_buffer_to(
        &self,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
        buffer: &mut crate::pool::PooledBuffer,
    ) {
        let old = std::mem::replace(buffer, pool.acquire());
        response.push_buffer_range(old, self.response.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FramedMultilineChunk {
    Complete(CompleteMultilineWireChunk),
    Incomplete(IncompleteMultilineWireChunk),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FramedSingleLineChunk {
    response: Range<usize>,
    next_response_input: Range<usize>,
}

impl FramedSingleLineChunk {
    pub(crate) async fn write_from<W>(
        &self,
        writer: &mut W,
        buffer: &[u8],
        conn: &mut crate::stream::ConnectionStream,
    ) -> Result<u64, crate::session::response_buffer::ResponseTransferError>
    where
        W: AsyncWrite + Unpin,
    {
        let response = &buffer[self.response.clone()];
        if self.next_response_input.start < buffer.len() {
            conn.queue_pending_bytes_first(&buffer[self.next_response_input.clone()])
                .map_err(crate::session::response_buffer::ResponseTransferError::Io)?;
        }
        writer
            .write_all(response)
            .await
            .map_err(crate::session::response_buffer::ResponseTransferError::ClientDisconnect)?;
        crate::pool::buffer::record_non_owned_response_write_chunk(response.len());
        Ok(response.len() as u64)
    }

    fn push_from_buffer(
        &self,
        io_buffer: &mut crate::pool::PooledBuffer,
        conn: &mut crate::stream::ConnectionStream,
        response: &mut crate::pool::ChunkedResponse,
        pool: &crate::pool::BufferPool,
    ) -> Result<(), crate::session::response_buffer::ResponseTransferError> {
        let total_len = io_buffer.initialized();
        let old = std::mem::replace(io_buffer, pool.acquire());
        if self.next_response_input.start < total_len {
            conn.queue_pending_bytes_first(&old[self.next_response_input.clone()])
                .map_err(crate::session::response_buffer::ResponseTransferError::Io)?;
        }
        response.push_buffer_range(old, self.response.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FramedResponseForRequest {
    response: Range<usize>,
}

impl FramedResponseForRequest {
    #[must_use]
    fn backend_bytes<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        &source[self.response.clone()]
    }

    #[must_use]
    fn consumed(&self) -> usize {
        self.response.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendReplyBytes<'a> {
    CompletedTrackedReply(&'a [u8]),
    ForwardUntracked(&'a [u8]),
}

#[derive(Default, Debug)]
pub(crate) struct BackendReplyTracker {
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
pub struct MultilineFramer {
    data: [u8; TERMINATOR_TAIL_SIZE],
    len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FramingError {
    UnexpectedTrailingResponseBytes,
    BackendEof,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseWarning {
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
        framed: FramedSingleLineChunk,
    },
    Multiline {
        status: crate::protocol::StatusCode,
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
            return Ok(Self::Multiline { status });
        }

        Ok(Self::SingleLine {
            status,
            framed: FramedSingleLineChunk {
                response: 0..status_line_end,
                next_response_input: status_line_end..chunk.len(),
            },
        })
    }

    #[inline]
    #[must_use]
    pub(crate) const fn status_code(&self) -> crate::protocol::StatusCode {
        match self {
            Self::SingleLine { status, .. } | Self::Multiline { status } => *status,
        }
    }

    #[must_use]
    pub(crate) fn single_line_bytes<'a>(&self, buffer: &'a [u8]) -> Option<&'a [u8]> {
        match self {
            Self::SingleLine { framed, .. } => Some(&buffer[framed.response.clone()]),
            Self::Multiline { .. } => None,
        }
    }

    #[must_use]
    pub(crate) fn complete_single_line_bytes<'a>(&self, buffer: &'a [u8]) -> Option<&'a [u8]> {
        match self {
            Self::SingleLine { framed, .. } if framed.next_response_input.is_empty() => {
                Some(&buffer[framed.response.clone()])
            }
            Self::SingleLine { .. } | Self::Multiline { .. } => None,
        }
    }
}

pub(crate) fn response_status(
    request: &crate::protocol::RequestContext,
    buffer: &crate::pool::PooledBuffer,
) -> Result<crate::protocol::StatusCode, ResponseReadError> {
    ResponseFrame::parse(request, buffer).map(|frame| frame.status_code())
}

pub(crate) fn single_line_response_bytes<'a>(
    request: &crate::protocol::RequestContext,
    bytes: &'a [u8],
) -> Result<Option<&'a [u8]>, ResponseReadError> {
    ResponseFrame::parse_bytes(request, bytes).map(|frame| frame.single_line_bytes(bytes))
}

pub(crate) fn complete_single_line_response_bytes<'a>(
    request: &crate::protocol::RequestContext,
    bytes: &'a [u8],
) -> Result<Option<&'a [u8]>, ResponseReadError> {
    ResponseFrame::parse_bytes(request, bytes).map(|frame| frame.complete_single_line_bytes(bytes))
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

pub(crate) fn log_response_warnings(
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

pub(crate) struct IsolatedMultilineResponse<'a> {
    conn: &'a mut crate::stream::ConnectionStream,
    io_buffer: &'a mut crate::pool::PooledBuffer,
}

impl<'a> IsolatedMultilineResponse<'a> {
    pub(crate) const fn new(
        conn: &'a mut crate::stream::ConnectionStream,
        io_buffer: &'a mut crate::pool::PooledBuffer,
    ) -> Self {
        Self { conn, io_buffer }
    }

    pub(crate) async fn capture_into(
        self,
        capture: &mut crate::pool::PooledBuffer,
    ) -> anyhow::Result<()> {
        let mut framer = MultilineFramer::default();
        let initial_len = self.io_buffer.initialized();
        let mut complete = match framer
            .frame_isolated_multiline_chunk(&self.io_buffer[..initial_len])
            .map_err(isolated_multiline_error)?
        {
            FramedMultilineChunk::Complete(complete) => {
                complete.extend_capture_from(&self.io_buffer[..initial_len], capture);
                true
            }
            FramedMultilineChunk::Incomplete(incomplete) => {
                incomplete.extend_capture_from(&self.io_buffer[..initial_len], capture);
                false
            }
        };

        while !complete {
            let n = self
                .io_buffer
                .read_from(self.conn)
                .await
                .context("Failed to read multiline response from backend")?;
            if n == 0 {
                anyhow::bail!("Backend closed connection before complete multiline response");
            }
            complete = match framer
                .frame_isolated_multiline_chunk(&self.io_buffer[..n])
                .map_err(isolated_multiline_error)?
            {
                FramedMultilineChunk::Complete(complete) => {
                    complete.extend_capture_from(&self.io_buffer[..n], capture);
                    true
                }
                FramedMultilineChunk::Incomplete(incomplete) => {
                    incomplete.extend_capture_from(&self.io_buffer[..n], capture);
                    false
                }
            };
        }
        Ok(())
    }

    pub(crate) async fn observe(self) -> Result<(), FramingError> {
        let mut framer = MultilineFramer::default();
        let initial_len = self.io_buffer.initialized();
        let mut complete = matches!(
            framer.frame_isolated_multiline_chunk(&self.io_buffer[..initial_len])?,
            FramedMultilineChunk::Complete(_)
        );

        while !complete {
            let n = self
                .io_buffer
                .read_from(self.conn)
                .await
                .map_err(|_| FramingError::Io)?;
            if n == 0 {
                return Err(FramingError::BackendEof);
            }
            complete = matches!(
                framer.frame_isolated_multiline_chunk(&self.io_buffer[..n])?,
                FramedMultilineChunk::Complete(_)
            );
        }
        Ok(())
    }

    pub(crate) async fn capture_chunked(
        self,
        pool: &crate::pool::BufferPool,
        response: &mut crate::pool::ChunkedResponse,
    ) -> Result<(), FramingError> {
        let mut framer = MultilineFramer::default();
        let initial_len = self.io_buffer.initialized();
        let mut complete =
            match framer.frame_isolated_multiline_chunk(&self.io_buffer[..initial_len])? {
                FramedMultilineChunk::Complete(complete) => {
                    complete.push_isolated_buffer_to(response, pool, self.io_buffer);
                    true
                }
                FramedMultilineChunk::Incomplete(incomplete) => {
                    incomplete.push_buffer_to(response, pool, self.io_buffer);
                    false
                }
            };

        while !complete {
            let n = self
                .io_buffer
                .read_from(self.conn)
                .await
                .map_err(|_| FramingError::Io)?;
            if n == 0 {
                return Err(FramingError::BackendEof);
            }
            complete = match framer.frame_isolated_multiline_chunk(&self.io_buffer[..n])? {
                FramedMultilineChunk::Complete(complete) => {
                    complete.push_isolated_buffer_to(response, pool, self.io_buffer);
                    true
                }
                FramedMultilineChunk::Incomplete(incomplete) => {
                    incomplete.push_buffer_to(response, pool, self.io_buffer);
                    false
                }
            };
        }
        Ok(())
    }
}

pub(crate) struct ResponseCapture<'a> {
    request: &'a crate::protocol::RequestContext,
    io_buffer: &'a mut crate::pool::PooledBuffer,
    conn: &'a mut crate::stream::ConnectionStream,
    response: &'a mut crate::pool::ChunkedResponse,
    pool: &'a crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
}

pub(crate) struct ResponseWrite<'a, W> {
    pub(crate) request: &'a crate::protocol::RequestContext,
    pub(crate) io_buffer: &'a mut crate::pool::PooledBuffer,
    pub(crate) conn: &'a mut crate::stream::ConnectionStream,
    pub(crate) writer: &'a mut W,
    pub(crate) backend_id: crate::types::BackendId,
}

impl<W> ResponseWrite<'_, W>
where
    W: AsyncWrite + Unpin,
{
    pub(crate) async fn write(
        self,
    ) -> Result<u64, crate::session::response_buffer::ResponseTransferError> {
        let ResponseWrite {
            request,
            io_buffer,
            conn,
            writer,
            backend_id,
        } = self;
        let mut framer = MultilineFramer::default();
        let initial_len = io_buffer.initialized();
        let frame = ResponseFrame::parse(request, io_buffer).map_err(|err| {
            crate::session::response_buffer::ResponseTransferError::Io(anyhow::anyhow!(
                "Failed to frame prefetched response: {err:?}"
            ))
        })?;

        if let ResponseFrame::SingleLine { framed, .. } = &frame {
            return framed
                .write_from(writer, &io_buffer[..initial_len], conn)
                .await;
        }

        let mut bytes_written = 0u64;
        let mut current_len = initial_len;
        loop {
            let (bytes, complete) = match framer.frame_multiline_chunk(&io_buffer[..current_len]) {
                FramedMultilineChunk::Complete(complete) => complete
                    .write_from(writer, &io_buffer[..current_len], conn)
                    .await
                    .map(|bytes| (bytes, true))?,
                FramedMultilineChunk::Incomplete(incomplete) => incomplete
                    .write_from(writer, &io_buffer[..current_len])
                    .await
                    .map(|bytes| (bytes, false))?,
            };
            bytes_written += bytes;
            if complete {
                return Ok(bytes_written);
            }

            current_len = match io_buffer.read_from(conn).await.map_err(|e| {
                crate::session::response_buffer::ResponseTransferError::Io(
                    anyhow::Error::from(e).context("Failed to read remaining response body"),
                )
            })? {
                0 => {
                    return Err(
                        crate::session::response_buffer::ResponseTransferError::BackendEof {
                            backend_id,
                            bytes_received: bytes_written,
                        },
                    );
                }
                n => n,
            };
        }
    }
}

impl<'a> ResponseCapture<'a> {
    pub(crate) const fn from_buffer(
        request: &'a crate::protocol::RequestContext,
        io_buffer: &'a mut crate::pool::PooledBuffer,
        conn: &'a mut crate::stream::ConnectionStream,
        response: &'a mut crate::pool::ChunkedResponse,
        pool: &'a crate::pool::BufferPool,
        backend_id: crate::types::BackendId,
    ) -> Self {
        Self {
            request,
            io_buffer,
            conn,
            response,
            pool,
            backend_id,
        }
    }

    pub(crate) async fn capture(
        self,
    ) -> Result<(), crate::session::response_buffer::ResponseTransferError> {
        let ResponseCapture {
            request,
            io_buffer,
            conn,
            response,
            pool,
            backend_id,
        } = self;
        let mut framer = MultilineFramer::default();
        let initial_len = io_buffer.initialized();
        let mut complete = false;
        let frame = ResponseFrame::parse(request, io_buffer).map_err(|err| {
            crate::session::response_buffer::ResponseTransferError::Io(anyhow::anyhow!(
                "Failed to frame prefetched response: {err:?}"
            ))
        })?;

        response.clear();
        if let ResponseFrame::SingleLine { framed, .. } = &frame {
            return framed.push_from_buffer(io_buffer, conn, response, pool);
        }

        match framer.frame_multiline_chunk(&io_buffer[..initial_len]) {
            FramedMultilineChunk::Complete(framed) => {
                framed.push_from_buffer(io_buffer, conn, response, pool, initial_len)?;
                complete = true;
            }
            FramedMultilineChunk::Incomplete(incomplete) => {
                incomplete.push_buffer_to(response, pool, io_buffer);
            }
        }

        while !complete {
            let n = io_buffer.read_from(conn).await.map_err(|e| {
                crate::session::response_buffer::ResponseTransferError::Io(
                    anyhow::Error::from(e).context("Failed to read remaining response body"),
                )
            })?;
            if n == 0 {
                return Err(
                    crate::session::response_buffer::ResponseTransferError::BackendEof {
                        backend_id,
                        bytes_received: response.len() as u64,
                    },
                );
            }
            match framer.frame_multiline_chunk(&io_buffer[..n]) {
                FramedMultilineChunk::Complete(framed) => {
                    framed.push_from_buffer(io_buffer, conn, response, pool, n)?;
                    complete = true;
                }
                FramedMultilineChunk::Incomplete(incomplete) => {
                    incomplete.push_buffer_to(response, pool, io_buffer);
                }
            }
        }
        Ok(())
    }
}

impl MultilineFramer {
    fn frame_multiline_chunk(&mut self, chunk: &[u8]) -> FramedMultilineChunk {
        self.split_chunk(chunk, PackedPendingBytesPolicy::AllowIfStatusPrefix)
            .expect("pending bytes policy cannot reject trailing bytes")
    }

    fn frame_isolated_multiline_chunk(
        &mut self,
        chunk: &[u8],
    ) -> Result<FramedMultilineChunk, FramingError> {
        self.split_chunk(chunk, PackedPendingBytesPolicy::Reject)
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
    ) -> Result<FramedMultilineChunk, FramingError> {
        for end in self.terminator_ends(chunk) {
            if end == chunk.len() {
                return Ok(FramedMultilineChunk::Complete(CompleteMultilineWireChunk {
                    response: 0..end,
                    next_response_input: end..end,
                }));
            }

            match suffix_policy {
                PackedPendingBytesPolicy::Reject => {
                    return Err(FramingError::UnexpectedTrailingResponseBytes);
                }
                PackedPendingBytesPolicy::AllowIfStatusPrefix
                    if plausible_status_prefix(&chunk[end..]) =>
                {
                    return Ok(FramedMultilineChunk::Complete(CompleteMultilineWireChunk {
                        response: 0..end,
                        next_response_input: end..chunk.len(),
                    }));
                }
                PackedPendingBytesPolicy::AllowIfStatusPrefix => {}
            }
        }

        self.update(chunk);
        Ok(FramedMultilineChunk::Incomplete(
            IncompleteMultilineWireChunk {
                response: 0..chunk.len(),
            },
        ))
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

    /// Return every complete terminator end offset touching the current chunk.
    #[must_use]
    fn terminator_ends(&self, chunk: &[u8]) -> smallvec::SmallVec<[usize; 2]> {
        let mut ends = smallvec::SmallVec::new();
        if let Some(end) = self.find_spanning_terminator(chunk) {
            ends.push(end);
        }
        ends.extend(terminator_ends(chunk));
        ends
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
    pub(crate) fn push_request(&mut self, kind: crate::protocol::RequestKind) {
        self.pending.push_back(PendingRequestFrame {
            kind,
            state: PendingRequestFrameState::AwaitingStatusLine,
            status_line: smallvec::SmallVec::new(),
        });
    }

    pub(crate) fn accept_backend_bytes<'a>(
        &mut self,
        chunk: &'a [u8],
    ) -> smallvec::SmallVec<[BackendReplyBytes<'a>; 4]> {
        let mut output = smallvec::SmallVec::new();
        let mut offset = 0;

        while offset < chunk.len() {
            let Some(front) = self.pending.front_mut() else {
                output.push(BackendReplyBytes::ForwardUntracked(&chunk[offset..]));
                break;
            };
            let Some(framed) = front.consume(chunk, offset) else {
                output.push(BackendReplyBytes::ForwardUntracked(&chunk[offset..]));
                break;
            };

            offset = framed.consumed();
            output.push(BackendReplyBytes::CompletedTrackedReply(
                framed.backend_bytes(chunk),
            ));
            self.pending.pop_front();
        }

        output
    }
}

impl PendingRequestFrame {
    fn consume(&mut self, chunk: &[u8], offset: usize) -> Option<FramedResponseForRequest> {
        match &mut self.state {
            PendingRequestFrameState::AwaitingStatusLine => {
                let Some(pos) = memchr::memchr(b'\n', &chunk[offset..]) else {
                    if self.status_line.len() + chunk[offset..].len()
                        > crate::constants::buffer::COMMAND
                    {
                        return Some(FramedResponseForRequest {
                            response: offset..chunk.len(),
                        });
                    }
                    self.status_line.extend_from_slice(&chunk[offset..]);
                    return None;
                };
                let end = offset + pos + 1;
                if self.status_line.len() + end - offset > crate::constants::buffer::COMMAND {
                    return Some(FramedResponseForRequest {
                        response: offset..end,
                    });
                }
                self.status_line.extend_from_slice(&chunk[offset..end]);
                let Some(status) = crate::protocol::StatusCode::parse(self.status_line.as_slice())
                else {
                    return Some(FramedResponseForRequest {
                        response: offset..end,
                    });
                };
                if !crate::protocol::request_kind_has_response_body(self.kind, status) {
                    return Some(FramedResponseForRequest {
                        response: offset..end,
                    });
                }

                let mut framer = MultilineFramer::default();
                framer.update(self.status_line.as_slice());
                self.status_line.clear();
                match framer
                    .split_chunk(&chunk[end..], PackedPendingBytesPolicy::AllowIfStatusPrefix)
                {
                    Ok(FramedMultilineChunk::Complete(complete)) => {
                        Some(FramedResponseForRequest {
                            response: offset..end + complete.response.end,
                        })
                    }
                    Ok(FramedMultilineChunk::Incomplete(_)) => {
                        self.state = PendingRequestFrameState::ReadingMultiline { framer };
                        None
                    }
                    Err(_) => Some(FramedResponseForRequest {
                        response: offset..chunk.len(),
                    }),
                }
            }
            PendingRequestFrameState::ReadingMultiline { framer } => {
                match framer.split_chunk(
                    &chunk[offset..],
                    PackedPendingBytesPolicy::AllowIfStatusPrefix,
                ) {
                    Ok(FramedMultilineChunk::Complete(complete)) => {
                        Some(FramedResponseForRequest {
                            response: offset..offset + complete.response.end,
                        })
                    }
                    Ok(FramedMultilineChunk::Incomplete(_)) => None,
                    Err(_) => Some(FramedResponseForRequest {
                        response: offset..chunk.len(),
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
        FramingError::BackendEof => {
            anyhow::anyhow!("Backend closed connection before complete multiline response")
        }
        FramingError::Io => anyhow::anyhow!("Failed to read multiline response from backend"),
    }
}

#[must_use]
pub(crate) fn complete_multiline_payload_split(
    payload: &[u8],
) -> Option<CompleteMultilinePayloadSplit> {
    if payload == b".\r\n" {
        return Some(CompleteMultilinePayloadSplit::new(0..0, 0..3));
    }
    let mut framer = MultilineFramer::default();
    match framer.split_chunk(payload, PackedPendingBytesPolicy::Reject) {
        Ok(FramedMultilineChunk::Complete(complete)) if complete.next_response_input.is_empty() => {
            let response_end = complete.response.end;
            let body_end = response_end.checked_sub(TERMINATOR.len())?;
            let terminator_start = response_end.checked_sub(3)?;
            Some(CompleteMultilinePayloadSplit::new(
                0..body_end,
                terminator_start..response_end,
            ))
        }
        Ok(FramedMultilineChunk::Complete(_))
        | Ok(FramedMultilineChunk::Incomplete(_))
        | Err(_) => None,
    }
}

#[must_use]
pub(crate) fn complete_multiline_payload_body(payload: &[u8]) -> Option<&[u8]> {
    complete_multiline_payload_split(payload).map(|split| &payload[split.body()])
}

#[must_use]
pub(crate) fn complete_response_body(payload: &[u8]) -> Option<&[u8]> {
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
fn terminator_ends(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    memchr::memmem::find_iter(data, TERMINATOR).map(|found| found + TERMINATOR.len())
}

#[inline]
#[cfg(test)]
fn find_terminator_end_from(data: &[u8], start: usize) -> Option<usize> {
    let data = data.get(start..)?;
    let mut first = None;
    for end in terminator_ends(data) {
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
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    const EXHAUSTIVE_BYTES: [u8; 4] = [b'\r', b'\n', b'.', b'x'];

    struct FailingWriter;

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

    async fn loopback_connection_stream() -> crate::stream::ConnectionStream {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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
            Ok(FramedMultilineChunk::Complete(CompleteMultilineWireChunk {
                response: 0..b"220 article\r\nbody\r\n.\r\n".len(),
                next_response_input: b"220 article\r\nbody\r\n.\r\n".len()
                    ..b"220 article\r\nbody\r\n.\r\n".len(),
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
            Ok(FramedMultilineChunk::Complete(CompleteMultilineWireChunk {
                response: 0..b"220 article\r\nbody\r\n.\r\n".len(),
                next_response_input: b"220 article\r\nbody\r\n.\r\n".len()..chunk.len(),
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
            Ok(FramedMultilineChunk::Complete(CompleteMultilineWireChunk {
                response: 0..chunk.len(),
                next_response_input: chunk.len()..chunk.len(),
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
            Ok(FramedMultilineChunk::Complete(CompleteMultilineWireChunk {
                response: 0..b".\r\n".len(),
                next_response_input: b".\r\n".len()..b".\r\n".len(),
            }))
        );
    }

    #[tokio::test]
    async fn complete_multiline_write_preserves_packed_suffix_before_client_error() {
        let chunk = b"220 article\r\nbody\r\n.\r\n223 0 <next>\r\n";
        let response_len = b"220 article\r\nbody\r\n.\r\n".len();
        let framed = CompleteMultilineWireChunk {
            response: 0..response_len,
            next_response_input: response_len..chunk.len(),
        };
        let mut conn = loopback_connection_stream().await;
        let mut writer = FailingWriter;

        let err = framed.write_from(&mut writer, chunk, &mut conn).await;

        assert!(matches!(
            err,
            Err(crate::session::response_buffer::ResponseTransferError::ClientDisconnect(_))
        ));
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
    }

    #[tokio::test]
    async fn single_line_write_preserves_packed_suffix_before_client_error() {
        let chunk = b"223 0 <first>\r\n223 0 <next>\r\n";
        let response_len = b"223 0 <first>\r\n".len();
        let framed = FramedSingleLineChunk {
            response: 0..response_len,
            next_response_input: response_len..chunk.len(),
        };
        let mut conn = loopback_connection_stream().await;
        let mut writer = FailingWriter;

        let err = framed.write_from(&mut writer, chunk, &mut conn).await;

        assert!(matches!(
            err,
            Err(crate::session::response_buffer::ResponseTransferError::ClientDisconnect(_))
        ));
        assert_eq!(conn.pending_bytes_len(), b"223 0 <next>\r\n".len());
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
    fn complete_multiline_payload_body_returns_body_without_terminator() {
        assert_eq!(complete_multiline_payload_body(b".\r\n"), Some(&b""[..]));
        assert_eq!(
            complete_multiline_payload_body(b"body\r\n.\r\n"),
            Some(&b"body"[..])
        );
        assert_eq!(complete_multiline_payload_body(b"body\r\n.\r\nextra"), None);
    }

    #[test]
    fn complete_multiline_payload_split_returns_body_and_terminator_ranges() {
        let payload = b"body\r\n.\r\n";

        let split = complete_multiline_payload_split(payload).expect("complete payload");

        assert_eq!(&payload[split.body()], b"body");
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
