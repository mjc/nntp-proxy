//! Benchmarks for NNTP multiline terminator detection at chunk boundaries.
//!
//! Existing response parsing benches cover complete buffers. These cases stress
//! cross-read boundaries and false positives, which matter for pipelined article
//! streaming and leftover correctness.
//!
//! Run with: cargo bench --bench tail_buffer_boundaries

use divan::{Bencher, black_box};
use nntp_proxy::session::streaming::tail_buffer::TailBuffer;

fn main() {
    divan::main();
}

fn detect_with_previous_tail(previous: &[u8], current: &[u8]) -> bool {
    let mut tail = TailBuffer::default();
    tail.update(previous);
    tail.detect_terminator(current).is_found()
}

fn large_body(size: usize, terminator: bool) -> Vec<u8> {
    let mut body = Vec::with_capacity(size + 5);
    body.extend(std::iter::repeat_n(b'x', size));
    if terminator {
        body.extend_from_slice(b"\r\n.\r\n");
    }
    body
}

#[inline]
fn naive_find_terminator(data: &[u8]) -> Option<usize> {
    data.windows(5)
        .position(|window| window == b"\r\n.\r\n")
        .map(|index| index + 5)
}

#[inline]
fn naive_detect_with_previous_tail(previous: &[u8], current: &[u8]) -> bool {
    let mut combined = Vec::with_capacity(previous.len() + current.len());
    combined.extend_from_slice(previous);
    combined.extend_from_slice(current);
    naive_find_terminator(&combined).is_some()
}

mod split_boundaries {
    use super::{Bencher, black_box, detect_with_previous_tail, naive_detect_with_previous_tail};

    macro_rules! bench_split {
        ($name:ident, $previous:literal, $current:literal) => {
            #[divan::bench(sample_count = 1000, sample_size = 1000)]
            fn $name(bencher: Bencher) {
                bencher.bench(|| {
                    black_box(detect_with_previous_tail(
                        black_box($previous),
                        black_box($current),
                    ))
                });
            }
        };
    }

    bench_split!(split_after_cr, b"\r", b"\n.\r\n");
    bench_split!(split_after_crlf, b"\r\n", b".\r\n");
    bench_split!(split_after_dot, b"\r\n.", b"\r\n");
    bench_split!(split_before_final_lf, b"\r\n.\r", b"\n");

    macro_rules! bench_naive_split {
        ($name:ident, $previous:literal, $current:literal) => {
            #[divan::bench(sample_count = 1000, sample_size = 1000)]
            fn $name(bencher: Bencher) {
                bencher.bench(|| {
                    black_box(naive_detect_with_previous_tail(
                        black_box($previous),
                        black_box($current),
                    ))
                });
            }
        };
    }

    bench_naive_split!(naive_split_after_cr, b"\r", b"\n.\r\n");
    bench_naive_split!(naive_split_after_crlf, b"\r\n", b".\r\n");
    bench_naive_split!(naive_split_after_dot, b"\r\n.", b"\r\n");
    bench_naive_split!(naive_split_before_final_lf, b"\r\n.\r", b"\n");
}

mod full_chunk_scans {
    use super::{Bencher, TailBuffer, black_box, large_body, naive_find_terminator};

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn false_positive_dot_stuffed_line(bencher: Bencher) {
        let chunk = b"body\r\n..not terminator\r\nmore body\r\n";
        let tail = TailBuffer::default();
        bencher.bench(|| black_box(tail.detect_terminator(black_box(chunk)).is_found()));
    }

    #[divan::bench(sample_count = 500, sample_size = 100)]
    fn no_terminator_64k(bencher: Bencher) {
        let chunk = large_body(64 * 1024, false);
        let tail = TailBuffer::default();
        bencher
            .counter(divan::counter::BytesCount::new(chunk.len()))
            .bench(|| black_box(tail.detect_terminator(black_box(&chunk)).is_found()));
    }

    #[divan::bench(sample_count = 500, sample_size = 100)]
    fn naive_no_terminator_64k(bencher: Bencher) {
        let chunk = large_body(64 * 1024, false);
        bencher
            .counter(divan::counter::BytesCount::new(chunk.len()))
            .bench(|| black_box(naive_find_terminator(black_box(&chunk))));
    }

    #[divan::bench(sample_count = 500, sample_size = 100)]
    fn terminator_at_end_64k(bencher: Bencher) {
        let chunk = large_body(64 * 1024, true);
        let tail = TailBuffer::default();
        bencher
            .counter(divan::counter::BytesCount::new(chunk.len()))
            .bench(|| black_box(tail.detect_terminator(black_box(&chunk)).is_found()));
    }

    #[divan::bench(sample_count = 500, sample_size = 100)]
    fn naive_terminator_at_end_64k(bencher: Bencher) {
        let chunk = large_body(64 * 1024, true);
        bencher
            .counter(divan::counter::BytesCount::new(chunk.len()))
            .bench(|| black_box(naive_find_terminator(black_box(&chunk))));
    }

    #[divan::bench(sample_count = 100, sample_size = 25)]
    fn terminator_at_end_1mb(bencher: Bencher) {
        let chunk = large_body(1024 * 1024, true);
        let tail = TailBuffer::default();
        bencher
            .counter(divan::counter::BytesCount::new(chunk.len()))
            .bench(|| black_box(tail.detect_terminator(black_box(&chunk)).is_found()));
    }

    #[divan::bench(sample_count = 100, sample_size = 25)]
    fn naive_terminator_at_end_1mb(bencher: Bencher) {
        let chunk = large_body(1024 * 1024, true);
        bencher
            .counter(divan::counter::BytesCount::new(chunk.len()))
            .bench(|| black_box(naive_find_terminator(black_box(&chunk))));
    }
}
