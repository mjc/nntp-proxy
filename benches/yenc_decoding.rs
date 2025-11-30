//! Benchmarks for yEnc decoding
//!
//! Measures performance of our custom yEnc decoder across:
//! - Different line lengths (typical yenc is 128-byte lines)
//! - Escaped vs non-escaped data
//! - Realistic binary data patterns
//!
//! Run with:
//! - Criterion: `cargo bench --bench yenc_decoding`
//! - Iai: `cargo bench --bench yenc_decoding_iai`

use divan::{Bencher, black_box};

fn main() {
    divan::main();
}

/// Generate yenc-encoded data of specified length
/// Uses realistic binary data distribution
fn generate_yenc_data(length: usize) -> Vec<u8> {
    (0..length)
        .map(|i| {
            let byte = (i % 256) as u8;
            // yenc encoding: (original + 42) % 256
            byte.wrapping_add(42)
        })
        .collect()
}

/// Generate yenc data with escapes (critical chars that need =X encoding)
fn generate_yenc_with_escapes(length: usize, escape_freq: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(length * 2);
    for i in 0..length {
        if i % escape_freq == 0 {
            // Escaped character
            data.push(b'=');
            let byte = (i % 256) as u8;
            data.push(byte.wrapping_add(64));
        } else {
            let byte = (i % 256) as u8;
            data.push(byte.wrapping_add(42));
        }
    }
    data
}

// =============================================================================
// Divan Benchmarks (fast, cachegrind-based)
// =============================================================================

mod divan_benches {
    use super::*;
    use nntp_proxy::protocol::yenc::decode_yenc_line;

    #[divan::bench(sample_count = 1000)]
    fn decode_128_bytes(bencher: Bencher) {
        let data = generate_yenc_data(128);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_256_bytes(bencher: Bencher) {
        let data = generate_yenc_data(256);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_512_bytes(bencher: Bencher) {
        let data = generate_yenc_data(512);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_1024_bytes(bencher: Bencher) {
        let data = generate_yenc_data(1024);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_with_10pct_escapes(bencher: Bencher) {
        let data = generate_yenc_with_escapes(128, 10);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_with_25pct_escapes(bencher: Bencher) {
        let data = generate_yenc_with_escapes(128, 4);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_with_50pct_escapes(bencher: Bencher) {
        let data = generate_yenc_with_escapes(128, 2);
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_with_crlf(bencher: Bencher) {
        let mut data = generate_yenc_data(128);
        data.extend_from_slice(b"\r\n");
        bencher.bench(|| black_box(decode_yenc_line(black_box(&data))));
    }

    #[divan::bench(sample_count = 1000)]
    fn decode_real_world_line(bencher: Bencher) {
        // Real yenc-encoded "Hello, yEnc!" with CRLF
        let data = b"r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n";
        bencher.bench(|| black_box(decode_yenc_line(black_box(data))));
    }
}
