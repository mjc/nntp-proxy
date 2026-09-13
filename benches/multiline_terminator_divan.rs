//! RAM-style wall-clock comparison for the fixed NNTP multiline terminator.

use divan::{Bencher, black_box, counter::BytesCount};
use nntp_proxy::session::multiline_framing::{
    benchmark_ashwa, benchmark_memchr_convenience, benchmark_memchr_finder,
};
use std::sync::LazyLock;

const STREAM_BYTES: usize = 8 * 1024 * 1024;
const ARTICLES: usize = 8;
const TERMINATOR: &[u8] = b"\r\n.\r\n";

static PAYLOAD: LazyLock<Box<[u8]>> = LazyLock::new(build_payload);

fn main() {
    divan::main();
}

fn build_payload() -> Box<[u8]> {
    let mut stream = Vec::with_capacity(STREAM_BYTES);
    for article in 0..ARTICLES {
        stream.extend_from_slice(format!("220 {article} <bench-{article}@example>\r\nSubject: scanner benchmark {article}\r\n\r\n").as_bytes());
        let end = (article + 1) * (STREAM_BYTES / ARTICLES) - TERMINATOR.len();
        while stream.len() < end {
            let byte = b'a' + ((stream.len() + article * 17) % 26) as u8;
            stream.push(byte);
            if stream.len() % 79 == 0 {
                stream.extend_from_slice(b"\r\n..dot-stuffed\r\n");
            }
        }
        stream.truncate(end);
        stream.extend_from_slice(TERMINATOR);
    }
    assert_eq!(stream.len(), STREAM_BYTES);
    assert_eq!(benchmark_memchr_convenience(&stream), ARTICLES);
    stream.into_boxed_slice()
}

#[cfg(target_arch = "x86_64")]
fn evict_from_cache(data: &[u8]) {
    unsafe {
        // SAFETY: every address is in `data`; CLFLUSH accepts unaligned addresses.
        for offset in (0..data.len()).step_by(64) {
            std::arch::x86_64::_mm_clflush(data.as_ptr().add(offset).cast());
        }
        std::arch::x86_64::_mm_mfence();
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn evict_from_cache(_data: &[u8]) {}

fn evicted_payload() -> &'static [u8] {
    evict_from_cache(&PAYLOAD);
    &PAYLOAD
}

#[divan::bench(sample_count = 300, sample_size = 1, counter = BytesCount::new(STREAM_BYTES))]
fn memchr_convenience(bencher: Bencher) {
    bencher
        .with_inputs(evicted_payload)
        .bench_local_values(|data| black_box(benchmark_memchr_convenience(black_box(data))));
}

#[divan::bench(sample_count = 300, sample_size = 1, counter = BytesCount::new(STREAM_BYTES))]
fn memchr_finder(bencher: Bencher) {
    bencher
        .with_inputs(evicted_payload)
        .bench_local_values(|data| black_box(benchmark_memchr_finder(black_box(data))));
}

#[divan::bench(sample_count = 300, sample_size = 1, counter = BytesCount::new(STREAM_BYTES))]
fn ashwa(bencher: Bencher) {
    bencher
        .with_inputs(evicted_payload)
        .bench_local_values(|data| black_box(benchmark_ashwa(black_box(data))));
}
