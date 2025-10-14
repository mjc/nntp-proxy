//! Tail buffer for tracking the last few bytes of streamed data
//!
//! Used to detect NNTP terminators that span across chunk boundaries.

use crate::protocol::{NntpResponse, TERMINATOR_TAIL_SIZE};

/// Status of terminator detection in a chunk
#[derive(Debug, Clone, Copy)]
pub(super) enum TerminatorStatus {
    /// Complete terminator found at this position (position is after terminator)
    FoundAt(usize),
    /// Terminator spans the boundary between tail and current chunk
    Spanning,
    /// No terminator found
    NotFound,
}

impl TerminatorStatus {
    /// Returns true if a terminator was found (either complete or spanning)
    pub(super) fn is_found(self) -> bool {
        !matches!(self, Self::NotFound)
    }
    /// Get the number of bytes to write from the chunk
    pub(super) fn write_len(self, chunk_size: usize) -> usize {
        match self {
            Self::FoundAt(pos) => pos,
            Self::Spanning | Self::NotFound => chunk_size,
        }
    }
}

/// Helper for tracking the last few bytes of streamed data
///
/// Used to detect terminators that span across chunk boundaries.
#[derive(Default)]
pub(super) struct TailBuffer {
    data: [u8; TERMINATOR_TAIL_SIZE],
    len: usize,
}

impl TailBuffer {
    pub(super) fn new() -> Self {
        Self::default()
    }
    /// Update tail with the last bytes from a chunk
    pub(super) fn update(&mut self, chunk: &[u8]) {
        if chunk.len() >= TERMINATOR_TAIL_SIZE {
            self.data
                .copy_from_slice(&chunk[chunk.len() - TERMINATOR_TAIL_SIZE..]);
            self.len = TERMINATOR_TAIL_SIZE;
        } else if !chunk.is_empty() {
            self.data[..chunk.len()].copy_from_slice(chunk);
            self.len = chunk.len();
        }
    }
    /// Get the tail data as a slice
    pub(super) fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }
    /// Get the current length of valid tail data
    pub(super) fn len(&self) -> usize {
        self.len
    }
    /// Returns true if the buffer is empty
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Check if terminator spans the boundary between this tail and the given chunk
    pub(super) fn has_spanning_terminator(&self, chunk: &[u8]) -> bool {
        // Early return if buffer is empty - no boundary to span
        if self.is_empty() {
            return false;
        }
        NntpResponse::has_spanning_terminator(self.as_slice(), self.len(), chunk, chunk.len())
    }
    /// Detect terminator in chunk, considering possible boundary spanning
    pub(super) fn detect_terminator(&self, chunk: &[u8]) -> TerminatorStatus {
        if let Some(pos) = NntpResponse::find_terminator_end(chunk) {
            TerminatorStatus::FoundAt(pos)
        } else if self.has_spanning_terminator(chunk) {
            TerminatorStatus::Spanning
        } else {
            TerminatorStatus::NotFound
        }
    }
}
