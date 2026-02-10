//! TCP command pipelining for per-command routing
//!
//! When a client sends multiple commands in a single TCP buffer (common with NZB
//! downloaders batching STAT/ARTICLE commands), this module reads them as a batch
//! so they can be processed without blocking on socket reads between each command.
//!
//! Single-command batches fall through to the existing sequential path with zero overhead.

use crate::command::NntpCommand;
use crate::session::ClientSession;
use anyhow::Result;
use tokio::io::AsyncBufReadExt;

/// Maximum pipeline depth (number of commands read from client buffer at once)
const MAX_PIPELINE_DEPTH: usize = 16;

/// A batch of commands read from the client's TCP buffer.
///
/// Uses an accumulator buffer pattern to avoid per-command allocations:
/// - All commands stored contiguously in a single String buffer
/// - Offsets track command boundaries (end position of each command)
/// - SmallVec keeps offsets on stack for common case (≤4 commands)
pub(super) struct CommandBatch {
    /// All commands accumulated in a single buffer
    buffer: String,
    /// End offset of each pipelineable command (offsets[i] = end of command i+1)
    offsets: smallvec::SmallVec<[usize; 4]>,
    /// Offset range for trailing non-pipelineable command if present
    trailing_range: Option<(usize, usize)>,
}

impl CommandBatch {
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

    /// Get the trailing non-pipelineable command if present
    pub fn trailing(&self) -> Option<&str> {
        self.trailing_range
            .map(|(start, end)| &self.buffer[start..end])
    }

    /// Number of pipelineable commands
    pub fn len(&self) -> usize {
        self.offsets.len()
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
    /// commands are read non-blocking from the BufReader's userspace buffer —
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
    ) -> Result<CommandBatch> {
        // Prepare accumulator buffer
        batch_buf.clear();
        batch_offsets.clear();
        let mut trailing_range: Option<(usize, usize)> = None;

        // First command: blocking read (must wait for client)
        command_buf.clear();
        match reader.read_line(command_buf).await {
            Ok(0) => {
                return Ok(CommandBatch {
                    buffer: String::new(),
                    offsets: smallvec::SmallVec::new(),
                    trailing_range: None,
                });
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }

        let parsed = NntpCommand::parse(command_buf);

        if !parsed.is_pipelineable() {
            // Single non-pipelineable command → return as trailing
            let start = 0;
            batch_buf.push_str(command_buf);
            let end = batch_buf.len();
            trailing_range = Some((start, end));
            return Ok(CommandBatch {
                buffer: std::mem::take(batch_buf),
                offsets: smallvec::SmallVec::new(),
                trailing_range,
            });
        }

        // Accumulate first pipelineable command
        batch_buf.push_str(command_buf);
        batch_offsets.push(batch_buf.len());

        // Read more commands from the buffer (non-blocking)
        while batch_offsets.len() < MAX_PIPELINE_DEPTH {
            if reader.buffer().is_empty() {
                break;
            }

            command_buf.clear();
            match reader.read_line(command_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let parsed = NntpCommand::parse(command_buf);
                    if !parsed.is_pipelineable() {
                        // Non-pipelineable command ends the batch
                        let start = batch_buf.len();
                        batch_buf.push_str(command_buf);
                        let end = batch_buf.len();
                        trailing_range = Some((start, end));
                        break;
                    }
                    batch_buf.push_str(command_buf);
                    batch_offsets.push(batch_buf.len());
                }
                Err(_) => break,
            }
        }

        Ok(CommandBatch {
            buffer: std::mem::take(batch_buf),
            offsets: std::mem::take(batch_offsets),
            trailing_range,
        })
    }
}
