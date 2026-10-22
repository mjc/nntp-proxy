//! Compare autovectorized fixed-pattern scans with the production cached Finder.

use divan::{Bencher, black_box, counter::BytesCount};
use nntp_proxy::session::{
    ScannerBenchmarkInput, benchmark_cr_dot_64, benchmark_cr_or_128, benchmark_cr_or_256,
    benchmark_cr_or_cursor_128, benchmark_cr_or_cursor_256, benchmark_cr_pair_64,
    benchmark_cr_pair_128, benchmark_cr_pair_256, benchmark_lf_pair_64, benchmark_lf_pair_128,
    benchmark_memchr_scanner,
};
use std::sync::LazyLock;

static INPUTS: LazyLock<[ScannerBenchmarkInput; 13]> = LazyLock::new(|| {
    let yenc = ScannerBenchmarkInput::new_yenc(716_800, 1);
    let packed = ScannerBenchmarkInput::new_yenc(716_800, 2);
    let inputs = [
        ScannerBenchmarkInput::new(64 * 1024, 1),
        ScannerBenchmarkInput::new(1024 * 1024, 1),
        ScannerBenchmarkInput::new(1024 * 1024, 8),
        ScannerBenchmarkInput::new(1024 * 1024, 64),
        ScannerBenchmarkInput::new_yenc(716_800, 1),
        ScannerBenchmarkInput::new_yenc(768_000, 1),
        yenc.first_read(16 * 1024),
        yenc.final_read(16 * 1024),
        yenc.first_read(64 * 1024),
        yenc.final_read(64 * 1024),
        packed.first_read(1024 * 1024),
        ScannerBenchmarkInput::new_yenc(768_000, 24).first_read(16 * 1024 * 1024),
        ScannerBenchmarkInput::mixed_yenc(768_000, 24).first_read(16 * 1024 * 1024),
    ];
    for input in &inputs {
        assert_eq!(benchmark_memchr_scanner(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_dot_64(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_pair_64(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_pair_128(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_pair_256(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_or_128(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_or_256(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_or_cursor_128(input.bytes()), input.expected());
        assert_eq!(benchmark_cr_or_cursor_256(input.bytes()), input.expected());
        assert_eq!(benchmark_lf_pair_64(input.bytes()), input.expected());
        assert_eq!(benchmark_lf_pair_128(input.bytes()), input.expected());
    }
    inputs
});

fn main() {
    // Fixture construction, dispatch warmup and result checks precede timing.
    LazyLock::force(&INPUTS);
    divan::main();
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn memchr(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_memchr_scanner(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_dot_64(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_dot_64(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_pair_64(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_pair_64(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_pair_128(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_pair_128(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn lf_pair_64(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_lf_pair_64(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn lf_pair_128(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_lf_pair_128(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_or_128(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_or_128(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_or_256(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_or_256(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_or_cursor_128(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_or_cursor_128(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_or_cursor_256(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_or_cursor_256(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], sample_count = 200, sample_size = 1)]
fn cr_pair_256(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_cr_pair_256(black_box(input.bytes()))));
}
