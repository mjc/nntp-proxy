//! Multiline response framing for tracking terminators across chunk boundaries
//!
//! Used to detect NNTP terminators that span across chunk boundaries.

use crate::protocol::TERMINATOR_TAIL_SIZE;

const TERMINATOR: &[u8; 5] = b"\r\n.\r\n";

/// Helper for tracking the last few bytes of streamed data
///
/// Used to detect terminators that span across chunk boundaries.
#[derive(Default)]
pub struct MultilineFramer {
    data: [u8; TERMINATOR_TAIL_SIZE],
    len: usize,
}

impl MultilineFramer {
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
    pub fn next_terminator_end(&self, chunk: &[u8]) -> Option<usize> {
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
    pub fn advance_to_next_terminator_end(&mut self, chunk: &[u8]) -> Option<usize> {
        let pos = self.next_terminator_end(chunk);
        if pos.is_none() {
            self.update(chunk);
        }
        pos
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
/// Returns the earliest complete terminator when multiple responses are packed
/// into the same read buffer.
///
/// We intentionally do not restore the old suffix-only/boundary-only fast path
/// here. That split behavior was easy to misuse, and using it in buffered or
/// pipelined paths caused one response to absorb bytes from the next response
/// in the same read. The small extra scan cost is acceptable compared to the
/// correctness requirement that every caller gets the first real terminator.
#[inline]
pub fn find_terminator_end(data: &[u8]) -> Option<usize> {
    find_terminator_end_from(data, 0)
}

#[inline]
fn find_terminator_end_from(data: &[u8], start: usize) -> Option<usize> {
    memchr::memmem::find(data.get(start..)?, TERMINATOR)
        .map(|found| start + found + TERMINATOR.len())
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

    const EXHAUSTIVE_BYTES: [u8; 4] = [b'\r', b'\n', b'.', b'x'];

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
    fn find_terminator_end_finds_first_complete_terminator() {
        let data = b"222 1 <a@b>\r\nbody-1\r\n.\r\n222 2 <c@d>\r\nbody-2\r\n.\r\n";
        let end = find_terminator_end(data).expect("first terminator should be found");

        assert_eq!(end, b"222 1 <a@b>\r\nbody-1\r\n.\r\n".len());
        assert_eq!(&data[end - 5..end], TERMINATOR);
        assert!(end < data.len());
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
