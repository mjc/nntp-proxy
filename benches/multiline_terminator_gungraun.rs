//! Deterministic instruction-count comparison for the same scanner kernels.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use gungraun::{library_benchmark, library_benchmark_group, main};
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use nntp_proxy::session::multiline_framing::{
    benchmark_ashwa, benchmark_memchr_convenience, benchmark_memchr_finder,
};
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use std::hint::black_box;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use std::sync::LazyLock;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const STREAM_BYTES: usize = 8 * 1024 * 1024;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const TERMINATOR: &[u8] = b"\r\n.\r\n";

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
static PAYLOAD: LazyLock<Box<[u8]>> = LazyLock::new(|| {
    let mut stream = Vec::with_capacity(STREAM_BYTES);
    for article in 0..8 {
        stream.extend_from_slice(
            format!(
                "220 {article} <bench-{article}@example>\r\nSubject: scanner benchmark {article}\r\n\r\n"
            )
            .as_bytes(),
        );
        let end = (article + 1) * (STREAM_BYTES / 8) - TERMINATOR.len();
        while stream.len() < end {
            stream.push(b'a' + ((stream.len() + article * 17) % 26) as u8);
            if stream.len() % 79 == 0 {
                stream.extend_from_slice(b"\r\n..dot-stuffed\r\n");
            }
        }
        stream.truncate(end);
        stream.extend_from_slice(TERMINATOR);
    }
    assert_eq!(stream.len(), STREAM_BYTES);
    stream.into_boxed_slice()
});

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn memchr_convenience() -> usize {
    black_box(benchmark_memchr_convenience(black_box(&PAYLOAD)))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn memchr_finder() -> usize {
    black_box(benchmark_memchr_finder(black_box(&PAYLOAD)))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[library_benchmark]
fn ashwa() -> usize {
    black_box(benchmark_ashwa(black_box(&PAYLOAD)))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
library_benchmark_group!(name = multiline_terminator; benchmarks = memchr_convenience, memchr_finder, ashwa);
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
main!(library_benchmark_groups = multiline_terminator);
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {}
