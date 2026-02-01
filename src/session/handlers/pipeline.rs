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
pub(super) struct CommandBatch {
    /// Pipelineable commands that can be processed sequentially
    pub commands: Vec<String>,
    /// If the last command was non-pipelineable, it's stored here
    /// to be processed sequentially after the batch.
    pub trailing_non_pipelineable: Option<String>,
}

impl CommandBatch {
    /// Whether this batch is empty (client disconnected)
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.trailing_non_pipelineable.is_none()
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
    pub(super) async fn read_command_batch(
        &self,
        reader: &mut tokio::io::BufReader<tokio::net::tcp::ReadHalf<'_>>,
        command_buf: &mut String,
    ) -> Result<CommandBatch> {
        let mut commands = Vec::with_capacity(4);
        let mut trailing = None;

        // First command: blocking read (must wait for client)
        command_buf.clear();
        match reader.read_line(command_buf).await {
            Ok(0) => {
                return Ok(CommandBatch {
                    commands,
                    trailing_non_pipelineable: trailing,
                });
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }

        let first_cmd = command_buf.clone();
        let parsed = NntpCommand::parse(&first_cmd);

        if !parsed.is_pipelineable() {
            // Single non-pipelineable command → return as trailing
            trailing = Some(first_cmd);
            return Ok(CommandBatch {
                commands,
                trailing_non_pipelineable: trailing,
            });
        }

        commands.push(first_cmd);

        // Read more commands from the buffer (non-blocking)
        while commands.len() < MAX_PIPELINE_DEPTH {
            if reader.buffer().is_empty() {
                break;
            }

            command_buf.clear();
            match reader.read_line(command_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let cmd = command_buf.clone();
                    let parsed = NntpCommand::parse(&cmd);
                    if !parsed.is_pipelineable() {
                        // Non-pipelineable command ends the batch
                        trailing = Some(cmd);
                        break;
                    }
                    commands.push(cmd);
                }
                Err(_) => break,
            }
        }

        Ok(CommandBatch {
            commands,
            trailing_non_pipelineable: trailing,
        })
    }
}
