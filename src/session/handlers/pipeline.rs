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
/// Uses an accumulator buffer pattern to avoid per-command allocations:
/// - All commands stored contiguously in a single String buffer
/// - Offsets track command boundaries (end position of each command)
/// - `SmallVec` keeps offsets on stack for common case (≤4 commands)
pub(super) struct RequestBatch {
    /// All commands accumulated in a single buffer
    buffer: String,
    /// End offset of each pipelineable command (offsets[i] = end of command i+1)
    offsets: smallvec::SmallVec<[usize; 4]>,
    /// Typed contexts for each pipelineable command.
    contexts: smallvec::SmallVec<[RequestContext; 4]>,
    /// Offset range for trailing non-pipelineable command if present
    trailing_range: Option<(usize, usize)>,
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
        self.offsets.is_empty() && self.trailing_range.is_none()
    }

    /// Get a command by index from the pipelineable commands
    pub fn command(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.offsets[i - 1] };
        let end = self.offsets[i];
        &self.buffer[start..end]
    }

    /// Get a typed context by index from the pipelineable commands.
    pub fn context(&self, i: usize) -> &RequestContext {
        &self.contexts[i]
    }

    /// Get the trailing non-pipelineable command if present
    pub fn trailing(&self) -> Option<&str> {
        self.trailing_range
            .map(|(start, end)| &self.buffer[start..end])
    }

    /// Get the trailing typed context if present.
    pub fn trailing_context(&self) -> Option<&RequestContext> {
        self.trailing_context.as_ref()
    }

    /// Number of pipelineable commands
    pub fn len(&self) -> usize {
        self.offsets.len()
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

    /// Extract buffers for reuse in next batch (move out, leaving empty)
    pub fn into_buffers(self) -> (String, smallvec::SmallVec<[usize; 4]>) {
        (self.buffer, self.offsets)
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
    /// Uses accumulator buffer to avoid per-command allocations.
    pub(super) async fn read_command_batch(
        &self,
        reader: &mut tokio::io::BufReader<tokio::net::tcp::ReadHalf<'_>>,
        command_buf: &mut String,
        batch_buf: &mut String,
        batch_offsets: &mut smallvec::SmallVec<[usize; 4]>,
    ) -> Result<RequestBatch> {
        // Prepare accumulator buffer
        batch_buf.clear();
        batch_offsets.clear();
        let mut trailing_range: Option<(usize, usize)> = None;
        let mut trailing_oversized = false;

        // First command: blocking read (must wait for client)
        command_buf.clear();
        match reader.read_line(command_buf).await {
            Ok(0) => {
                return Ok(RequestBatch {
                    buffer: String::new(),
                    offsets: smallvec::SmallVec::new(),
                    contexts: smallvec::SmallVec::new(),
                    trailing_range: None,
                    trailing_context: None,
                    trailing_oversized: false,
                    first_oversized: false,
                });
            }
            Ok(_) => {
                // RFC 3977 §3.1: 512-byte command limit — return 501 and keep session alive
                if command_buf.len() > 512 {
                    return Ok(RequestBatch {
                        buffer: String::new(),
                        offsets: smallvec::SmallVec::new(),
                        contexts: smallvec::SmallVec::new(),
                        trailing_range: None,
                        trailing_context: None,
                        trailing_oversized: false,
                        first_oversized: true,
                    });
                }
            }
            Err(e) => return Err(e.into()),
        }

        let request = RequestContext::from_request_line(command_buf);

        if !request.is_pipelineable() {
            // Single non-pipelineable command → return as trailing
            let start = 0;
            batch_buf.push_str(command_buf);
            let end = batch_buf.len();
            trailing_range = Some((start, end));
            let trailing_context = Some(request);
            return Ok(RequestBatch {
                buffer: std::mem::take(batch_buf),
                offsets: smallvec::SmallVec::new(),
                contexts: smallvec::SmallVec::new(),
                trailing_range,
                trailing_context,
                trailing_oversized: false,
                first_oversized: false,
            });
        }

        // Accumulate first pipelineable command
        batch_buf.push_str(command_buf);
        batch_offsets.push(batch_buf.len());
        let mut batch_contexts: smallvec::SmallVec<[RequestContext; 4]> = smallvec::SmallVec::new();
        batch_contexts.push(request);

        // Read more commands from the buffer (non-blocking)
        while batch_offsets.len() < MAX_PIPELINE_DEPTH {
            // Only proceed if buffer has a complete line (contains \n).
            // Checking just is_empty() is insufficient: if the buffer has a partial
            // command without \n, read_line() would block on the socket waiting for
            // more data, defeating the non-blocking batch intent.
            if memchr::memchr(b'\n', reader.buffer()).is_none() {
                break;
            }

            command_buf.clear();
            match reader.read_line(command_buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // M4: Reject oversized commands (end batch on invalid command)
                    // Mark as oversized so caller sends 500 error instead of forwarding
                    if command_buf.len() > 512 {
                        let start = batch_buf.len();
                        batch_buf.push_str(command_buf);
                        let end = batch_buf.len();
                        trailing_range = Some((start, end));
                        let trailing_context = Some(RequestContext::from_request_line(command_buf));
                        trailing_oversized = true;
                        return Ok(RequestBatch {
                            buffer: std::mem::take(batch_buf),
                            offsets: std::mem::take(batch_offsets),
                            contexts: batch_contexts,
                            trailing_range,
                            trailing_context,
                            trailing_oversized,
                            first_oversized: false,
                        });
                    }
                    let request = RequestContext::from_request_line(command_buf);
                    if !request.is_pipelineable() {
                        // Non-pipelineable command ends the batch
                        let start = batch_buf.len();
                        batch_buf.push_str(command_buf);
                        let end = batch_buf.len();
                        trailing_range = Some((start, end));
                        let trailing_context = Some(request);
                        return Ok(RequestBatch {
                            buffer: std::mem::take(batch_buf),
                            offsets: std::mem::take(batch_offsets),
                            contexts: batch_contexts,
                            trailing_range,
                            trailing_context,
                            trailing_oversized,
                            first_oversized: false,
                        });
                    }
                    batch_buf.push_str(command_buf);
                    batch_offsets.push(batch_buf.len());
                    batch_contexts.push(request);
                }
            }
        }

        Ok(RequestBatch {
            buffer: std::mem::take(batch_buf),
            offsets: std::mem::take(batch_offsets),
            contexts: batch_contexts,
            trailing_range,
            trailing_context: None,
            trailing_oversized,
            first_oversized: false,
        })
    }
}
