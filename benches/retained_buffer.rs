//! Retained-buffer append policy benchmarks.
//!
//! Run with:
//! `nix develop -c cargo bench --features framing-bench --bench retained_buffer`

use divan::{Bencher, black_box};
use nntp_proxy::pool::buffer::retained_append_benchmark;
use tokio::runtime::Builder;

fn main() {
    divan::main();
}

fn bench_tail(bencher: Bencher, physical_tail: usize) {
    let runtime = Builder::new_current_thread().build().unwrap();
    bencher
        .with_inputs(|| retained_append_benchmark(physical_tail))
        .bench_local_values(|case| black_box(runtime.block_on(case.drain())));
}

#[divan::bench(sample_count = 100, sample_size = 100)]
fn exact_full(bencher: Bencher) {
    bench_tail(bencher, 0);
}

#[divan::bench(sample_count = 100, sample_size = 100)]
fn one_byte_tail(bencher: Bencher) {
    bench_tail(bencher, 1);
}

#[divan::bench(sample_count = 100, sample_size = 100)]
fn roomy_tail(bencher: Bencher) {
    bench_tail(bencher, 4096);
}
