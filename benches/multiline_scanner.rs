//! Compare fixed-pattern SIMD kernels against the production cached Finder.

use divan::{Bencher, black_box, counter::BytesCount};
use nntp_proxy::session::{
    ScannerBenchmarkInput, benchmark_auto_cr_mask, benchmark_auto_lf_mask, benchmark_auto_pair_16,
    benchmark_auto_pair_32, benchmark_auto_pair_64, benchmark_fixed_scanner,
    benchmark_memchr_scanner,
};
use std::sync::LazyLock;

static INPUTS: LazyLock<[ScannerBenchmarkInput; 4]> = LazyLock::new(|| {
    let inputs = [
        ScannerBenchmarkInput::new(64 * 1024, 1),
        ScannerBenchmarkInput::new(1024 * 1024, 1),
        ScannerBenchmarkInput::new(1024 * 1024, 8),
        ScannerBenchmarkInput::new(1024 * 1024, 64),
    ];
    for input in &inputs {
        assert_eq!(benchmark_memchr_scanner(input.bytes()), input.expected());
        assert_eq!(benchmark_fixed_scanner(input.bytes()), input.expected());
        assert_eq!(benchmark_auto_pair_16(input.bytes()), input.expected());
        assert_eq!(benchmark_auto_pair_32(input.bytes()), input.expected());
        assert_eq!(benchmark_auto_pair_64(input.bytes()), input.expected());
        assert_eq!(benchmark_auto_cr_mask(input.bytes()), input.expected());
        assert_eq!(benchmark_auto_lf_mask(input.bytes()), input.expected());
    }
    inputs
});

fn main() {
    // Fixture construction, dispatch warmup and result checks precede timing.
    LazyLock::force(&INPUTS);
    divan::main();
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn memchr(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_memchr_scanner(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn fixed(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_fixed_scanner(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn auto_pair_16(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_auto_pair_16(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn auto_pair_32(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_auto_pair_32(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn auto_pair_64(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_auto_pair_64(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn auto_cr_mask(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_auto_cr_mask(black_box(input.bytes()))));
}

#[divan::bench(args = [0, 1, 2, 3], sample_count = 200, sample_size = 1)]
fn auto_lf_mask(bencher: Bencher, input: usize) {
    let input = &INPUTS[input];
    bencher
        .counter(BytesCount::new(input.bytes().len()))
        .bench(|| black_box(benchmark_auto_lf_mask(black_box(input.bytes()))));
}
