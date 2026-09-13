//! RAM timing: eight next-terminator searches over one 8 MiB packed stream.
//! Setup flushes the input before EACH sample. Keep sample_size = 1:
//! Divan prepares all inputs in a sample before timing that sample.

#[cfg(target_arch = "x86_64")]
use divan::{Bencher, black_box};
#[cfg(target_arch = "x86_64")]
use nntp_proxy::session::multiline_framing_bench::{
    finder, flush_input, packed_stream, scan_ashwa, scan_finder, scan_memchr, validate,
};

#[cfg(target_arch = "x86_64")]
fn main() {
    divan::main();
}

#[cfg(not(target_arch = "x86_64"))]
fn main() {
    panic!("RAM timing requires x86_64 CLFLUSH");
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(sample_count = 300, sample_size = 1)]
fn memchr_convenience_packed_8m(bencher: Bencher) {
    let data = packed_stream();
    validate(&data, &finder());
    bencher
        .with_inputs(|| flush_input(&data))
        .bench_local_values(|()| black_box(scan_memchr(black_box(&data))));
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(sample_count = 300, sample_size = 1)]
fn memchr_finder_packed_8m(bencher: Bencher) {
    let data = packed_stream();
    let finder = finder();
    validate(&data, &finder);
    bencher
        .with_inputs(|| flush_input(&data))
        .bench_local_values(|()| black_box(scan_finder(black_box(&data), black_box(&finder))));
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(sample_count = 300, sample_size = 1)]
fn ashwa_packed_8m(bencher: Bencher) {
    let data = packed_stream();
    validate(&data, &finder());
    bencher
        .with_inputs(|| flush_input(&data))
        .bench_local_values(|()| black_box(scan_ashwa(black_box(&data))));
}
