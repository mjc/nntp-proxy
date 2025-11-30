//! Tail buffer for tracking the last few bytes of streamed data
//!
//! Used to detect NNTP terminators that span across chunk boundaries.

use crate::protocol::TERMINATOR_TAIL_SIZE;

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
        has_spanning_terminator(self.as_slice(), self.len(), chunk, chunk.len())
    }
    /// Detect terminator in chunk, considering possible boundary spanning
    ///
    /// **Performance**: find_terminator_end() checks end first (O(1)),
    /// only scans with memchr if terminator is mid-chunk (rare).
    /// This optimizes the 99% case where terminator is at chunk end.
    pub(super) fn detect_terminator(&self, chunk: &[u8]) -> TerminatorStatus {
        if let Some(pos) = find_terminator_end(chunk) {
            TerminatorStatus::FoundAt(pos)
        } else if self.has_spanning_terminator(chunk) {
            TerminatorStatus::Spanning
        } else {
            TerminatorStatus::NotFound
        }
    }
}

/// Find the position of the NNTP multiline terminator in data
///
/// Returns the position AFTER the terminator (exclusive end), or None if not found.
/// This handles the case where extra data appears after the terminator in the same chunk.
///
/// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
/// the terminator is exactly "\r\n.\r\n" (CRLF, dot, CRLF).
///
/// **Optimization**: Uses `memchr::memchr_iter()` to find '\r' bytes (SIMD-accelerated),
/// then validates the full 5-byte pattern. This eliminates the need to create new slices
/// on each iteration (which the manual loop approach requires with `&data[pos..]`).
///
/// Benchmarks show this is **72% faster for small responses** (37ns → 13ns) and
/// **64% faster for medium responses** (109ns → 40ns) compared to the manual loop
/// that creates a new slice on each iteration.
///
/// **Hot path optimization**: Check end first (99% case for streaming chunks),
/// then scan forward if needed. Compiler optimizes slice comparison to memcmp.
#[inline]
fn find_terminator_end(data: &[u8]) -> Option<usize> {
    const TERMINATOR: [u8; 5] = *b"\r\n.\r\n";

    // Fast path: check suffix, or slow path: scan for terminator mid-chunk
    data.len()
        .checked_sub(5)
        .filter(|&start| data[start..] == TERMINATOR)
        .map(|_| data.len())
        .or_else(|| {
            memchr::memchr_iter(b'\r', data)
                .take_while(|&pos| pos + 5 <= data.len())
                .find(|&pos| data[pos..pos + 5] == TERMINATOR)
                .map(|pos| pos + 5)
        })
}

/// Check if a terminator spans across a boundary between tail and current chunk
///
/// This handles the case where a multiline terminator is split across two read chunks.
/// For example: previous chunk ends with "\r\n." and current starts with "\r\n"
///
/// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
/// the terminator is exactly "\r\n.\r\n" (CRLF, dot, CRLF).
#[inline]
fn has_spanning_terminator(
    tail: &[u8],
    tail_len: usize,
    current: &[u8],
    current_len: usize,
) -> bool {
    // Only check if we have a tail and current chunk is small enough for spanning
    if tail_len < 1 || !(1..=4).contains(&current_len) {
        return false;
    }

    // Check all possible split positions of the 5-byte terminator "\r\n.\r\n"
    // Split after byte 1: tail ends with "\r", current starts with "\n.\r\n"
    if tail_len >= 1
        && current_len >= 4
        && tail[tail_len - 1] == b'\r'
        && current[..4] == *b"\n.\r\n"
    {
        return true;
    }
    // Split after byte 2: tail ends with "\r\n", current starts with ".\r\n"
    if tail_len >= 2
        && current_len >= 3
        && tail[tail_len - 2..tail_len] == *b"\r\n"
        && current[..3] == *b".\r\n"
    {
        return true;
    }
    // Split after byte 3: tail ends with "\r\n.", current starts with "\r\n"
    if tail_len >= 3
        && current_len >= 2
        && tail[tail_len - 3..tail_len] == *b"\r\n."
        && current[..2] == *b"\r\n"
    {
        return true;
    }
    // Split after byte 4: tail ends with "\r\n.\r", current starts with "\n"
    if tail_len >= 4
        && current_len >= 1
        && tail[tail_len - 4..tail_len] == *b"\r\n.\r"
        && current[0] == b'\n'
    {
        return true;
    }

    false
}

/// Check if data ends with the NNTP multiline terminator
///
/// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1):
/// ```text
/// Multiline blocks are terminated by a line containing only a period:
/// CRLF "." CRLF
/// Which appears in the data stream as: \r\n.\r\n
/// ```
///
/// **Optimization**: Single suffix check, no scanning.
#[inline]
pub(crate) fn has_terminator_at_end(data: &[u8]) -> bool {
    let n = data.len();
    // Only check for proper RFC 3977 terminator: \r\n.\r\n
    n >= 5 && data[n - 5..n] == *b"\r\n.\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    // TerminatorStatus tests

    #[test]
    fn test_terminator_status_is_found() {
        assert!(TerminatorStatus::FoundAt(10).is_found());
        assert!(TerminatorStatus::Spanning.is_found());
        assert!(!TerminatorStatus::NotFound.is_found());
    }

    #[test]
    fn test_terminator_status_write_len() {
        assert_eq!(TerminatorStatus::FoundAt(42).write_len(100), 42);
        assert_eq!(TerminatorStatus::Spanning.write_len(100), 100);
        assert_eq!(TerminatorStatus::NotFound.write_len(100), 100);
    }

    #[test]
    fn test_terminator_status_write_len_zero() {
        assert_eq!(TerminatorStatus::FoundAt(0).write_len(50), 0);
    }

    // TailBuffer::update tests

    #[test]
    fn test_tail_buffer_update_large_chunk() {
        let mut tail = TailBuffer::default();
        let chunk = b"This is a long message with more than 4 bytes\r\n.\r\n";
        tail.update(chunk);

        // Should store last 4 bytes of "...\r\n.\r\n" which is "\n.\r\n"
        assert_eq!(tail.len(), TERMINATOR_TAIL_SIZE);
        assert_eq!(tail.as_slice(), b"\n.\r\n");
    }

    #[test]
    fn test_tail_buffer_update_small_chunk() {
        let mut tail = TailBuffer::default();
        let chunk = b"Hi";
        tail.update(chunk);

        assert_eq!(tail.len(), 2);
        assert_eq!(tail.as_slice(), b"Hi");
    }

    #[test]
    fn test_tail_buffer_update_exact_size() {
        let mut tail = TailBuffer::default();
        let chunk = b".\r\n"; // 3 bytes, less than TERMINATOR_TAIL_SIZE (4)
        tail.update(chunk);

        assert_eq!(tail.len(), 3);
        assert_eq!(tail.as_slice(), b".\r\n");
    }

    #[test]
    fn test_tail_buffer_update_empty_chunk() {
        let mut tail = TailBuffer::default();
        tail.update(b"initial");
        let initial_len = tail.len();

        tail.update(b"");
        assert_eq!(tail.len(), initial_len); // Should not change
    }

    #[test]
    fn test_tail_buffer_update_successive() {
        let mut tail = TailBuffer::default();
        tail.update(b"first chunk with terminator\r\n");
        let first_tail = tail.as_slice().to_vec();

        tail.update(b"second chunk");
        // Should completely replace with last 5 bytes of "second chunk"
        assert_ne!(tail.as_slice(), first_tail.as_slice());
    }

    // TailBuffer state methods tests

    #[test]
    fn test_tail_buffer_is_empty() {
        let tail = TailBuffer::default();
        assert!(tail.is_empty());

        let mut tail = TailBuffer::default();
        tail.update(b"data");
        assert!(!tail.is_empty());
    }

    #[test]
    fn test_tail_buffer_len() {
        let tail = TailBuffer::default();
        assert_eq!(tail.len(), 0);

        let mut tail = TailBuffer::default();
        tail.update(b"AB");
        assert_eq!(tail.len(), 2);

        tail.update(b"long string with more bytes");
        assert_eq!(tail.len(), TERMINATOR_TAIL_SIZE);
    }

    // TailBuffer::has_spanning_terminator tests

    #[test]
    fn test_has_spanning_terminator_empty_tail() {
        let tail = TailBuffer::default();
        assert!(!tail.has_spanning_terminator(b"\r\n.\r\n"));
    }

    #[test]
    fn test_has_spanning_terminator_no_match() {
        let mut tail = TailBuffer::default();
        tail.update(b"abcde");
        assert!(!tail.has_spanning_terminator(b"fghij"));
    }

    #[test]
    fn test_has_spanning_terminator_complete_in_chunk() {
        let mut tail = TailBuffer::default();
        tail.update(b"data\r");
        // Terminator completely in next chunk
        assert!(!tail.has_spanning_terminator(b"\n.\r\nmore"));
    }

    #[test]
    fn test_has_spanning_terminator_actual_span() {
        // Test case 1: tail ends with "\r\n", current starts with ".\r\n" (3 bytes)
        let mut tail = TailBuffer::default();
        tail.update(b"data\r\n"); // Last 4 bytes: "ta\r\n"
        // Spanning requires current_len to be 1-4 bytes
        assert!(tail.has_spanning_terminator(b".\r\n"));

        // Test case 2: tail ends with "\r\n.", current starts with "\r\n" (2 bytes)
        let mut tail2 = TailBuffer::default();
        tail2.update(b"line\r\n."); // Last 4 bytes: "e\r\n."
        assert!(tail2.has_spanning_terminator(b"\r\n"));

        // Test case 3: tail ends with "\r", current starts with "\n.\r\n" (4 bytes)
        let mut tail3 = TailBuffer::default();
        tail3.update(b"text\r"); // Last 4 bytes: "ext\r"
        assert!(tail3.has_spanning_terminator(b"\n.\r\n"));

        // Test case 4: tail ends with "\r\n.\r", current starts with "\n" (1 byte)
        let mut tail4 = TailBuffer::default();
        tail4.update(b"abc\r\n.\r"); // Last 4 bytes: "\r\n.\r"
        assert!(tail4.has_spanning_terminator(b"\n"));
    }

    // TailBuffer::detect_terminator tests

    #[test]
    fn test_detect_terminator_found_at_end() {
        let tail = TailBuffer::default();
        let chunk = b"Article content here\r\n.\r\n";
        let status = tail.detect_terminator(chunk);

        match status {
            TerminatorStatus::FoundAt(pos) => assert_eq!(pos, chunk.len()),
            _ => panic!("Expected FoundAt, got {:?}", status),
        }
    }

    #[test]
    fn test_detect_terminator_found_mid_chunk() {
        let tail = TailBuffer::default();
        let chunk = b"Line 1\r\n.\r\nExtra data";
        let status = tail.detect_terminator(chunk);

        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert!(pos < chunk.len());
                assert!(pos > 0);
            }
            _ => panic!("Expected FoundAt, got {:?}", status),
        }
    }

    #[test]
    fn test_detect_terminator_spanning() {
        let mut tail = TailBuffer::default();
        // Set up tail to end with part of terminator
        tail.update(b"article content\r\n");
        // Next chunk starts with rest of terminator
        let chunk = b".\r\n";
        let status = tail.detect_terminator(chunk);

        assert!(matches!(status, TerminatorStatus::Spanning));
    }

    #[test]
    fn test_detect_terminator_not_found() {
        let mut tail = TailBuffer::default();
        tail.update(b"previous");
        let chunk = b"current chunk no terminator";
        let status = tail.detect_terminator(chunk);

        assert!(matches!(status, TerminatorStatus::NotFound));
    }

    #[test]
    fn test_detect_terminator_empty_chunk() {
        let tail = TailBuffer::default();
        let status = tail.detect_terminator(b"");
        assert!(matches!(status, TerminatorStatus::NotFound));
    }

    // Integration-style tests

    #[test]
    fn test_tail_buffer_workflow() {
        let mut tail = TailBuffer::default();

        // First chunk
        let chunk1 = b"Header: value\r\n\r\nBody content";
        tail.update(chunk1);
        assert_eq!(tail.len(), TERMINATOR_TAIL_SIZE);
        assert!(!tail.detect_terminator(chunk1).is_found());

        // Second chunk with terminator
        let chunk2 = b" more body\r\n.\r\n";
        let status = tail.detect_terminator(chunk2);
        assert!(status.is_found());
    }

    #[test]
    fn test_write_len_calculation() {
        // Test write_len() for different scenarios
        let chunk_size = 1024;

        // Terminator at position 500
        assert_eq!(TerminatorStatus::FoundAt(500).write_len(chunk_size), 500);

        // Spanning or not found - write full chunk
        assert_eq!(TerminatorStatus::Spanning.write_len(chunk_size), chunk_size);
        assert_eq!(TerminatorStatus::NotFound.write_len(chunk_size), chunk_size);
    }
}
