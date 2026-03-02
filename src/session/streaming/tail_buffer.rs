//! Tail buffer for tracking the last few bytes of streamed data
//!
//! Used to detect NNTP terminators that span across chunk boundaries.

use crate::protocol::TERMINATOR_TAIL_SIZE;

/// Status of terminator detection in a chunk
#[derive(Debug, Clone, Copy)]
pub enum TerminatorStatus {
    /// Complete terminator found at this position (position is after terminator)
    FoundAt(usize),
    /// No terminator found
    NotFound,
}

impl TerminatorStatus {
    /// Returns true if a terminator was found
    pub const fn is_found(self) -> bool {
        matches!(self, Self::FoundAt(_))
    }
    /// Get the number of bytes to write from the chunk
    pub const fn write_len(self, chunk_size: usize) -> usize {
        match self {
            Self::FoundAt(pos) => pos,
            Self::NotFound => chunk_size,
        }
    }
}

/// Helper for tracking the last few bytes of streamed data
///
/// Used to detect terminators that span across chunk boundaries.
#[derive(Default)]
pub struct TailBuffer {
    data: [u8; TERMINATOR_TAIL_SIZE],
    len: usize,
}

impl TailBuffer {
    /// Update tail with the last bytes from a chunk
    ///
    /// Maintains the last `TERMINATOR_TAIL_SIZE` bytes of the concatenation of
    /// all prior chunks. When `chunk` is smaller than `TERMINATOR_TAIL_SIZE`,
    /// the prior tail bytes are shifted to preserve the rolling window — not
    /// overwritten — so terminators split across three or more tiny reads
    /// (e.g. `\r\n`, `.`, `\r\n`) are correctly detected.
    pub fn update(&mut self, chunk: &[u8]) {
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
    /// Get the tail data as a slice
    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }
    /// Get the current length of valid tail data
    pub const fn len(&self) -> usize {
        self.len
    }
    /// Returns true if the buffer is empty
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Find spanning terminator offset in chunk
    ///
    /// Returns the byte offset in the chunk where the terminator ends,
    /// or None if no spanning terminator is found.
    pub fn find_spanning_terminator(&self, chunk: &[u8]) -> Option<usize> {
        // Early return if buffer is empty - no boundary to span
        if self.is_empty() {
            return None;
        }
        find_spanning_terminator(self.as_slice(), self.len(), chunk, chunk.len())
    }
    /// Detect terminator in chunk, considering possible boundary spanning
    ///
    /// **Performance**: `find_terminator_end()` checks end first (O(1)),
    /// only scans with memchr if terminator is mid-chunk (rare).
    /// This optimizes the 99% case where terminator is at chunk end.
    pub fn detect_terminator(&self, chunk: &[u8]) -> TerminatorStatus {
        if let Some(pos) = find_terminator_end(chunk) {
            TerminatorStatus::FoundAt(pos)
        } else if let Some(end) = self.find_spanning_terminator(chunk) {
            TerminatorStatus::FoundAt(end)
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

    // TerminatorStatus tests

    #[test]
    fn test_terminator_status_is_found() {
        assert!(TerminatorStatus::FoundAt(10).is_found());
        assert!(!TerminatorStatus::NotFound.is_found());
    }

    #[test]
    fn test_terminator_status_write_len() {
        assert_eq!(TerminatorStatus::FoundAt(42).write_len(100), 42);
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

    /// Regression test: update() with successive small chunks must maintain the
    /// rolling window — not overwrite it — so terminators split across 3+ reads work.
    #[test]
    fn test_tail_buffer_update_successive_small_chunks() {
        // Terminator "\r\n.\r\n" split as: ["\r\n"] ["."] ["\r\n"]
        let mut tail = TailBuffer::default();

        tail.update(b"\r\n"); // len=2, tail=['\r','\n']
        assert_eq!(tail.as_slice(), b"\r\n");

        tail.update(b"."); // len=3, tail=['\r','\n','.']
        assert_eq!(tail.as_slice(), b"\r\n.");

        // Now detect_terminator on the final chunk should find spanning match
        let status = tail.detect_terminator(b"\r\n");
        assert!(
            status.is_found(),
            "TailBuffer must detect terminator split across 3 reads"
        );
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

    // TailBuffer::find_spanning_terminator tests

    #[test]
    fn test_find_spanning_terminator_empty_tail() {
        let tail = TailBuffer::default();
        assert_eq!(tail.find_spanning_terminator(b"\r\n.\r\n"), None);
    }

    #[test]
    fn test_find_spanning_terminator_no_match() {
        let mut tail = TailBuffer::default();
        tail.update(b"abcde");
        assert_eq!(tail.find_spanning_terminator(b"fghij"), None);
    }

    #[test]
    fn test_find_spanning_terminator_complete_in_chunk() {
        let mut tail = TailBuffer::default();
        tail.update(b"data\r");
        // Terminator completely in next chunk, not spanning
        // This used to incorrectly return false due to chunk size guard.
        // Now it correctly returns Some(4) because tail ends with \r and chunk starts with \n.\r\n
        assert_eq!(tail.find_spanning_terminator(b"\n.\r\nmore"), Some(4));
    }

    #[test]
    fn test_find_spanning_terminator_actual_span() {
        // Test case 1: tail ends with "\r\n", current starts with ".\r\n" (3 bytes)
        let mut tail = TailBuffer::default();
        tail.update(b"data\r\n"); // Last 4 bytes: "ta\r\n"
        assert_eq!(tail.find_spanning_terminator(b".\r\n"), Some(3));

        // Test case 2: tail ends with "\r\n.", current starts with "\r\n" (2 bytes)
        let mut tail2 = TailBuffer::default();
        tail2.update(b"line\r\n."); // Last 4 bytes: "e\r\n."
        assert_eq!(tail2.find_spanning_terminator(b"\r\n"), Some(2));

        // Test case 3: tail ends with "\r", current starts with "\n.\r\n" (4 bytes)
        let mut tail3 = TailBuffer::default();
        tail3.update(b"text\r"); // Last 4 bytes: "ext\r"
        assert_eq!(tail3.find_spanning_terminator(b"\n.\r\n"), Some(4));

        // Test case 4: tail ends with "\r\n.\r", current starts with "\n" (1 byte)
        let mut tail4 = TailBuffer::default();
        tail4.update(b"abc\r\n.\r"); // Last 4 bytes: "\r\n.\r"
        assert_eq!(tail4.find_spanning_terminator(b"\n"), Some(1));
    }

    // TailBuffer::detect_terminator tests

    #[test]
    fn test_detect_terminator_found_at_end() {
        let tail = TailBuffer::default();
        let chunk = b"Article content here\r\n.\r\n";
        let status = tail.detect_terminator(chunk);

        match status {
            TerminatorStatus::FoundAt(pos) => assert_eq!(pos, chunk.len()),
            TerminatorStatus::NotFound => panic!("Expected FoundAt, got {status:?}"),
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
            TerminatorStatus::NotFound => panic!("Expected FoundAt, got {status:?}"),
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

        match status {
            TerminatorStatus::FoundAt(pos) => assert_eq!(pos, 3),
            _ => panic!("Expected FoundAt(3), got {status:?}"),
        }
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

        // Not found - write full chunk
        assert_eq!(TerminatorStatus::NotFound.write_len(chunk_size), chunk_size);
    }

    // =========================================================================
    // Property tests for TailBuffer terminator detection
    // =========================================================================

    #[test]
    fn prop_find_terminator_end_never_panics_empty() {
        let data = b"";
        let _ = find_terminator_end(data);
    }

    #[test]
    fn prop_find_terminator_end_never_panics_small() {
        for i in 1..=4 {
            let data = vec![b'x'; i];
            let _ = find_terminator_end(&data);
        }
    }

    #[test]
    fn prop_find_terminator_end_never_panics_large() {
        let data = vec![b'x'; 10000];
        let _ = find_terminator_end(&data);
    }

    #[test]
    fn prop_find_terminator_end_detects_at_end() {
        let data = b"article content\r\n.\r\n";
        let result = find_terminator_end(data);

        assert!(result.is_some());
        assert_eq!(result.unwrap(), data.len());
    }

    #[test]
    fn prop_find_terminator_end_detects_mid_chunk() {
        let data = b"line1\r\nline2\r\n.\r\nline3";
        let result = find_terminator_end(data);

        assert!(result.is_some());
        let pos = result.unwrap();
        assert!(pos < data.len());
        assert!(pos >= 5); // Position of terminator end
        assert_eq!(data[pos - 5..pos], *b"\r\n.\r\n");
    }

    #[test]
    fn prop_find_terminator_end_rejects_incomplete() {
        let data = b"line\r\n."; // Incomplete terminator
        let result = find_terminator_end(data);

        assert!(result.is_none());
    }

    #[test]
    fn prop_find_terminator_end_rejects_false_positives() {
        let data = b"data with \r\n but no dot"; // CRLF but no dot
        let result = find_terminator_end(data);

        assert!(result.is_none());
    }

    #[test]
    fn prop_find_spanning_terminator_never_panics_all_splits() {
        // Test all 4 possible split positions with expected offsets
        let terminators = vec![
            (b"\r".to_vec(), b"\n.\r\n".to_vec(), 4), // Split 1
            (b"\r\n".to_vec(), b".\r\n".to_vec(), 3), // Split 2
            (b"\r\n.".to_vec(), b"\r\n".to_vec(), 2), // Split 3
            (b"\r\n.\r".to_vec(), b"\n".to_vec(), 1), // Split 4
        ];

        for (tail_bytes, chunk_bytes, expected_offset) in terminators {
            let result = find_spanning_terminator(
                &tail_bytes,
                tail_bytes.len(),
                &chunk_bytes,
                chunk_bytes.len(),
            );
            assert_eq!(
                result,
                Some(expected_offset),
                "Failed for tail={:?}, chunk={:?}",
                std::str::from_utf8(&tail_bytes),
                std::str::from_utf8(&chunk_bytes)
            );
        }
    }

    #[test]
    fn prop_find_spanning_terminator_rejects_non_spanning() {
        // These should NOT be detected as spanning
        let non_spanning = vec![
            (b"abc".to_vec(), b"def".to_vec()),
            (b"\r".to_vec(), b"abc".to_vec()),
            (b"\r\n".to_vec(), b"abc".to_vec()),
            (b"".to_vec(), b"\r\n.\r\n".to_vec()), // Empty tail
        ];

        for (tail_bytes, chunk_bytes) in non_spanning {
            let result = find_spanning_terminator(
                &tail_bytes,
                tail_bytes.len(),
                &chunk_bytes,
                chunk_bytes.len(),
            );
            assert_eq!(
                result,
                None,
                "False positive for tail={:?}, chunk={:?}",
                std::str::from_utf8(&tail_bytes),
                std::str::from_utf8(&chunk_bytes)
            );
        }
    }

    #[test]
    fn prop_tail_buffer_update_preserves_last_n_bytes() {
        let mut tail = TailBuffer::default();

        // Update with small chunk (< TERMINATOR_TAIL_SIZE)
        let small = b"ab";
        tail.update(small);
        assert_eq!(tail.as_slice(), small);
        assert_eq!(tail.len(), 2);

        // Update with larger chunk (>= TERMINATOR_TAIL_SIZE)
        let large = b"0123456789abcdefghijklmnop";
        tail.update(large);
        assert_eq!(tail.len(), TERMINATOR_TAIL_SIZE);
        // Should have last TERMINATOR_TAIL_SIZE bytes
        let expected_start = large.len() - TERMINATOR_TAIL_SIZE;
        assert_eq!(tail.as_slice(), &large[expected_start..]);
    }

    #[test]
    fn prop_tail_buffer_is_empty_initially() {
        let tail = TailBuffer::default();
        assert!(tail.is_empty());
        assert_eq!(tail.len(), 0);
    }

    #[test]
    fn prop_tail_buffer_after_update_not_empty() {
        let mut tail = TailBuffer::default();
        tail.update(b"x");
        assert!(!tail.is_empty());
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn prop_tail_buffer_detect_terminator_consistency() {
        let mut tail = TailBuffer::default();

        // Case 1: No terminator in either chunk
        tail.update(b"no terminator here");
        let status = tail.detect_terminator(b"more data");
        assert!(matches!(status, TerminatorStatus::NotFound));

        // Case 2: Terminator at end
        let tail = TailBuffer::default();
        let status = tail.detect_terminator(b"content\r\n.\r\n");
        assert!(matches!(status, TerminatorStatus::FoundAt(_)));

        // Case 3: Terminator mid-chunk
        let tail = TailBuffer::default();
        let status = tail.detect_terminator(b"content\r\n.\r\nextra");
        assert!(matches!(status, TerminatorStatus::FoundAt(_)));
    }

    #[test]
    fn prop_spanning_junction_multiple_boundaries() {
        let mut tail = TailBuffer::default();

        // Test each of the 4 split positions with expected offsets
        // Split 1: tail ends with "\r", chunk starts with "\n.\r\n" → offset 4
        tail.update(b"text\r");
        assert_eq!(tail.find_spanning_terminator(b"\n.\r\n"), Some(4));

        // Split 2: tail ends with "\r\n", chunk starts with ".\r\n" → offset 3
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n");
        assert_eq!(tail.find_spanning_terminator(b".\r\n"), Some(3));

        // Split 3: tail ends with "\r\n.", chunk starts with "\r\n" → offset 2
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n.");
        assert_eq!(tail.find_spanning_terminator(b"\r\n"), Some(2));

        // Split 4: tail ends with "\r\n.\r", chunk starts with "\n" → offset 1
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n.\r");
        assert_eq!(tail.find_spanning_terminator(b"\n"), Some(1));
    }

    // =========================================================================
    // NEW TESTS for spanning terminator offset detection
    // =========================================================================

    #[test]
    fn test_spanning_with_large_chunk() {
        // Critical bug: spanning detection rejects chunks > 4 bytes
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n"); // tail ends "\r\n"

        // Chunk: ".\r\n" + 8000 bytes of data (real-world TCP read size)
        let mut chunk = b".\r\n".to_vec();
        chunk.extend(vec![b'X'; 8000]);

        let status = tail.detect_terminator(&chunk);
        // Should return FoundAt(3), not NotFound
        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert_eq!(pos, 3, "Terminator ends at byte 3");
            }
            _ => panic!("Expected FoundAt(3), got {status:?}"),
        }
    }

    #[test]
    fn test_spanning_offset_split1() {
        let mut tail = TailBuffer::default();
        tail.update(b"text\r"); // tail ends "\r"
        let chunk = b"\n.\r\nLeftover data here";

        let status = tail.detect_terminator(chunk);
        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert_eq!(pos, 4, "Split 1: terminator ends at byte 4");
            }
            _ => panic!("Expected FoundAt(4), got {status:?}"),
        }
    }

    #[test]
    fn test_spanning_offset_split2() {
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n"); // tail ends "\r\n"
        let chunk = b".\r\nLeftover data here";

        let status = tail.detect_terminator(chunk);
        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert_eq!(pos, 3, "Split 2: terminator ends at byte 3");
            }
            _ => panic!("Expected FoundAt(3), got {status:?}"),
        }
    }

    #[test]
    fn test_spanning_offset_split3() {
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n."); // tail ends "\r\n."
        let chunk = b"\r\nLeftover data here";

        let status = tail.detect_terminator(chunk);
        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert_eq!(pos, 2, "Split 3: terminator ends at byte 2");
            }
            _ => panic!("Expected FoundAt(2), got {status:?}"),
        }
    }

    #[test]
    fn test_spanning_offset_split4() {
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n.\r"); // tail ends "\r\n.\r"
        let chunk = b"\nLeftover data here";

        let status = tail.detect_terminator(chunk);
        match status {
            TerminatorStatus::FoundAt(pos) => {
                assert_eq!(pos, 1, "Split 4: terminator ends at byte 1");
            }
            _ => panic!("Expected FoundAt(1), got {status:?}"),
        }
    }

    #[test]
    fn test_spanning_write_len_not_full_chunk() {
        // Verify write_len returns offset, not full chunk size
        let mut tail = TailBuffer::default();
        tail.update(b"text\r\n");
        let chunk = b".\r\nLeftover123456789";

        let status = tail.detect_terminator(chunk);
        let write_len = status.write_len(chunk.len());

        assert_eq!(write_len, 3, "Should write only up to terminator end");
        assert!(write_len < chunk.len(), "Should not write full chunk");
    }
}
