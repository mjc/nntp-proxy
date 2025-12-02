//! Benchmarks for yEnc decoding
//!
//! Now that we use the yenc crate, these benchmarks just verify
//! we're getting expected performance from it.

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

mod yenc_crate_benches {
    use super::*;

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_128_bytes(bencher: Bencher) {
        let data = generate_yenc_data(128);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_256_bytes(bencher: Bencher) {
        let data = generate_yenc_data(256);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_512_bytes(bencher: Bencher) {
        let data = generate_yenc_data(512);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_1024_bytes(bencher: Bencher) {
        let data = generate_yenc_data(1024);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_4k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(4096);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_8k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(8192);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_16k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(16384);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 500)]
    fn yenc_decode_32k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(32768);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 500)]
    fn yenc_decode_64k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(65536);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 200)]
    fn yenc_decode_128k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(131072);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 200)]
    fn yenc_decode_256k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(262144);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 100)]
    fn yenc_decode_512k_bytes(bencher: Bencher) {
        let data = generate_yenc_data(524288);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 50)]
    fn yenc_decode_1mb_bytes(bencher: Bencher) {
        let data = generate_yenc_data(1048576);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_with_10pct_escapes(bencher: Bencher) {
        let data = generate_yenc_with_escapes(128, 10);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_with_25pct_escapes(bencher: Bencher) {
        let data = generate_yenc_with_escapes(128, 4);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_with_50pct_escapes(bencher: Bencher) {
        let data = generate_yenc_with_escapes(128, 2);
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_with_crlf(bencher: Bencher) {
        let mut data = generate_yenc_data(128);
        data.extend_from_slice(b"\r\n");
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(&data)).unwrap()));
    }

    #[divan::bench(sample_count = 1000)]
    fn yenc_decode_real_world_line(bencher: Bencher) {
        // Real yenc-encoded "Hello, yEnc!" with CRLF
        let data = b"r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n";
        bencher.bench(|| black_box(yenc::decode_buffer(black_box(data)).unwrap()));
    }
}
