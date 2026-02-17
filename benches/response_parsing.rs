//! Benchmarks for NNTP response parsing optimization

use divan::{Bencher, black_box};

fn main() {
    divan::main();
}

mod status_code_parsing {
    use super::*;
    use nntp_proxy::protocol::StatusCode;

    const RESPONSES: &[&[u8]] = &[
        b"200 Ready\r\n",
        b"220 0 12345 <msgid@example.com>\r\n",
        b"381 Password required\r\n",
    ];

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn optimized(bencher: Bencher) {
        bencher.bench(|| {
            for response in RESPONSES {
                black_box(StatusCode::parse(black_box(*response)));
            }
        });
    }
}

mod terminator_finding {
    use super::*;
    use nntp_proxy::session::streaming::tail_buffer::TailBuffer;

    /// Naive baseline: linear scan for terminator
    #[inline]
    fn find_terminator_old(data: &[u8]) -> Option<usize> {
        let n = data.len();
        if n < 5 {
            return None;
        }

        for i in 0..=(n - 5) {
            if &data[i..i + 5] == b"\r\n.\r\n" {
                return Some(i + 5);
            }
        }

        None
    }

    /// Production: TailBuffer with SIMD-accelerated detection
    ///
    /// Note: TailBuffer is stateful (tracks cross-boundary terminators),
    /// but for these complete-response benchmarks, we use empty state.
    #[inline]
    fn find_terminator_production(data: &[u8]) -> Option<usize> {
        use nntp_proxy::session::streaming::tail_buffer::TerminatorStatus;

        let tail = TailBuffer::default(); // Empty state - no previous chunks
        match tail.detect_terminator(data) {
            TerminatorStatus::FoundAt(pos) => Some(pos),
            TerminatorStatus::NotFound => None,
        }
    }

    const SMALL_RESPONSE: &[u8] =
        b"220 0 12345 <msgid@example.com>\r\nArticle content here\r\nMore lines\r\n.\r\n";

    const MEDIUM_RESPONSE: &[u8] = b"220 0 12345 <msgid@example.com>\r\n\
        Header: value\r\n\
        Another-Header: another value\r\n\
        \r\n\
        Article body line 1\r\n\
        Article body line 2\r\n\
        Article body line 3\r\n\
        Article body line 4\r\n\
        Article body line 5\r\n\
        .\r\n";

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn baseline_small(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_old(black_box(SMALL_RESPONSE))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn production_small(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_production(black_box(SMALL_RESPONSE))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn baseline_medium(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_old(black_box(MEDIUM_RESPONSE))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn production_medium(bencher: Bencher) {
        bencher.bench(|| black_box(find_terminator_production(black_box(MEDIUM_RESPONSE))));
    }
}
