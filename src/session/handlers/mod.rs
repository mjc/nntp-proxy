//! Session handlers for different routing modes
//!
//! This module contains the core session handling logic split by routing mode:
//! - `stateful`: Stateful 1:1 routing with dedicated backend connection
//! - `per_command`: Per-command routing where each command can go to a different backend (stateless)
//! - `hybrid`: Hybrid mode that starts with per-command routing and switches to stateful
//!
//! Shared utilities are in the parent `session::common` module.
//!
//! All handler functions are implemented as methods on `ClientSession` in their
//! respective modules. No need to re-export since they're all impl blocks.

mod article_retry;

/// Split a single-line pipelined response at its `\n` boundary in-place.
///
/// In a pipelined batch, a single TCP read may contain the single-line response
/// for the current command *and* the start of the next command's response.
/// This function truncates `response` to contain only the current response's
/// bytes (up to and including `\n`), and appends the remainder to `leftover`
/// for the next iteration.
///
/// This is distinct from [`TailBuffer`] terminator detection, which finds the
/// 5-byte `\r\n.\r\n` that ends a *multiline* response body.
///
/// [`TailBuffer`]: crate::session::streaming::tail_buffer::TailBuffer
pub(super) fn split_single_line_response(
    response: &mut crate::pool::PooledBuffer,
    leftover: &mut crate::pool::PooledBuffer,
) {
    if let Some(pos) = memchr::memchr(b'\n', response) {
        let end = pos + 1;
        if end < response.len() {
            leftover.extend_from_slice(&response[end..]);
        }
        response.truncate(end);
    }
}
mod cache_operations;
mod command_execution;
mod hybrid;
mod per_command;
mod pipeline;
pub(crate) mod pipeline_worker;
mod stateful;
