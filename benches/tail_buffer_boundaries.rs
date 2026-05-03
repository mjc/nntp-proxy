//! Benchmarks for NNTP multiline terminator detection at chunk boundaries.
//!
//! Existing response parsing benches cover complete buffers. These cases stress
//! cross-read boundaries and false positives, which matter for pipelined article
//! streaming and leftover correctness.
//!
//! Run with: cargo bench --bench `tail_buffer_boundaries`

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

macro_rules! bench_split {
    ($module:ident, $detect:ident, $(($name:ident, $previous:literal, $current:literal)),+ $(,)?) => {
        mod $module {
            use super::{Bencher, black_box, $detect};

            $(
                #[divan::bench(sample_count = 1000, sample_size = 1000)]
                fn $name(bencher: Bencher) {
                    bencher.bench(|| black_box($detect(black_box($previous), black_box($current))));
                }
            )+
        }
    };
}

bench_split!(
    split_boundaries,
    detect_with_previous_tail,
    (after_cr, b"\r", b"\n.\r\n"),
    (after_crlf, b"\r\n", b".\r\n"),
    (after_dot, b"\r\n.", b"\r\n"),
    (before_final_lf, b"\r\n.\r", b"\n"),
);

bench_split!(
    naive_split_boundaries,
    naive_detect_with_previous_tail,
    (after_cr, b"\r", b"\n.\r\n"),
    (after_crlf, b"\r\n", b".\r\n"),
    (after_dot, b"\r\n.", b"\r\n"),
    (before_final_lf, b"\r\n.\r", b"\n"),
);

mod full_chunk_scans {
    use super::{Bencher, TailBuffer, black_box, large_body, naive_find_terminator};

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn false_positive_dot_stuffed_line(bencher: Bencher) {
        let chunk = b"body\r\n..not terminator\r\nmore body\r\n";
        let tail = TailBuffer::default();
        bencher.bench(|| black_box(tail.detect_terminator(black_box(chunk)).is_found()));
    }

    macro_rules! bench_scan {
        ($name:ident, $size:expr, $terminator:expr, $samples:expr, $sample_size:expr, $body:expr) => {
            #[divan::bench(sample_count = $samples, sample_size = $sample_size)]
            fn $name(bencher: Bencher) {
                let chunk = large_body($size, $terminator);
                bencher
                    .counter(divan::counter::BytesCount::new(chunk.len()))
                    .bench(|| black_box($body(&chunk)));
            }
        };
    }

    fn tail_detect(chunk: &[u8]) -> bool {
        TailBuffer::default().detect_terminator(chunk).is_found()
    }

    bench_scan!(no_terminator_64k, 64 * 1024, false, 500, 100, tail_detect);
    bench_scan!(
        naive_no_terminator_64k,
        64 * 1024,
        false,
        500,
        100,
        naive_find_terminator
    );
    bench_scan!(
        terminator_at_end_64k,
        64 * 1024,
        true,
        500,
        100,
        tail_detect
    );
    bench_scan!(
        naive_terminator_at_end_64k,
        64 * 1024,
        true,
        500,
        100,
        naive_find_terminator
    );
    bench_scan!(
        terminator_at_end_1mb,
        1024 * 1024,
        true,
        100,
        25,
        tail_detect
    );
    bench_scan!(
        naive_terminator_at_end_1mb,
        1024 * 1024,
        true,
        100,
        25,
        naive_find_terminator
    );
}
