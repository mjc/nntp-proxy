//! Compare the proxy's incremental multiline framing with a stateless control.
//!
//! Run with:
//! `devenv shell -- cargo bench --features framing-bench --bench multiline_framing`

use divan::{Bencher, black_box, counter::BytesCount};
use nntp_proxy::session::{
    benchmark_incremental_multiline_frame, benchmark_multiline_response,
    benchmark_stateless_multiline_frame,
};
use std::sync::LazyLock;

fn main() {
    divan::main();
}

const BODY_64K: usize = 64 * 1024;
const BODY_768K: usize = 768 * 1024;

static RESPONSE_64K: LazyLock<Vec<u8>> = LazyLock::new(|| benchmark_multiline_response(BODY_64K));
static RESPONSE_768K: LazyLock<Vec<u8>> = LazyLock::new(|| benchmark_multiline_response(BODY_768K));

fn bench_incremental(bencher: Bencher, response: &[u8], chunk_size: usize) {
    bencher.counter(BytesCount::new(response.len())).bench(|| {
        black_box(benchmark_incremental_multiline_frame(
            black_box(response),
            chunk_size,
        ))
    });
}

fn bench_stateless(bencher: Bencher, response: &[u8], chunk_size: usize) {
    bencher.counter(BytesCount::new(response.len())).bench(|| {
        black_box(benchmark_stateless_multiline_frame(
            black_box(response),
            chunk_size,
        ))
    });
}

macro_rules! incremental_benchmarks {
    ($module:ident, $response:ident, [$(($name:ident, $chunk:expr)),+ $(,)?]) => {
        mod $module {
            use super::*;

            $(
                #[divan::bench(sample_count = 50, sample_size = 10)]
                fn $name(bencher: Bencher) {
                    bench_incremental(bencher, &$response, $chunk);
                }
            )+
        }
    };
}

macro_rules! stateless_benchmarks {
    ($module:ident, $response:ident, [$(($name:ident, $chunk:expr, $samples:expr)),+ $(,)?]) => {
        mod $module {
            use super::*;

            $(
                #[divan::bench(sample_count = $samples, sample_size = 5)]
                fn $name(bencher: Bencher) {
                    bench_stateless(bencher, &$response, $chunk);
                }
            )+
        }
    };
}

incremental_benchmarks!(
    incremental_64k,
    RESPONSE_64K,
    [
        (one_byte, 1),
        (two_bytes, 2),
        (four_bytes, 4),
        (thirty_one_bytes, 31),
        (chunk_256, 256),
        (whole_read, usize::MAX),
    ]
);

incremental_benchmarks!(
    incremental_768k,
    RESPONSE_768K,
    [
        (four_bytes, 4),
        (thirty_one_bytes, 31),
        (chunk_256k, 256 * 1024),
        (whole_read, usize::MAX),
    ]
);

stateless_benchmarks!(
    stateless_64k,
    RESPONSE_64K,
    [
        (one_byte, 1, 50),
        (two_bytes, 2, 50),
        (four_bytes, 4, 50),
        (thirty_one_bytes, 31, 50),
        (chunk_256, 256, 50),
        (whole_read, usize::MAX, 50),
    ]
);

stateless_benchmarks!(
    stateless_768k,
    RESPONSE_768K,
    [
        (thirty_one_bytes, 31, 10),
        (chunk_256k, 256 * 1024, 20),
        (whole_read, usize::MAX, 20),
    ]
);
