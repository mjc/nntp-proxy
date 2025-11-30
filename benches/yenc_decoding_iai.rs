//! Iai benchmarks for yEnc decoding (cachegrind-based, deterministic)
//!
//! Iai uses Valgrind's cachegrind to provide instruction counts and cache metrics.
//! Unlike timing-based benchmarks, these are deterministic and reproducible.
//!
//! Run with: `cargo bench --bench yenc_decoding_iai`

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use nntp_proxy::protocol::yenc::decode_yenc_line;

/// Generate yenc-encoded data of specified length
fn generate_yenc_data(length: usize) -> Vec<u8> {
    (0..length)
        .map(|i| {
            let byte = (i % 256) as u8;
            byte.wrapping_add(42)
        })
        .collect()
}

/// Generate yenc data with escapes
fn generate_yenc_with_escapes(length: usize, escape_freq: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(length * 2);
    for i in 0..length {
        if i % escape_freq == 0 {
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

#[library_benchmark]
#[bench::small(generate_yenc_data(128))]
#[bench::medium(generate_yenc_data(256))]
#[bench::large(generate_yenc_data(512))]
#[bench::xlarge(generate_yenc_data(1024))]
fn decode_various_sizes(data: Vec<u8>) -> Vec<u8> {
    decode_yenc_line(&data)
}

#[library_benchmark]
#[bench::no_escapes(generate_yenc_data(128))]
#[bench::few_escapes(generate_yenc_with_escapes(128, 10))]
#[bench::many_escapes(generate_yenc_with_escapes(128, 4))]
#[bench::half_escapes(generate_yenc_with_escapes(128, 2))]
fn decode_with_escapes(data: Vec<u8>) -> Vec<u8> {
    decode_yenc_line(&data)
}

#[library_benchmark]
fn decode_real_world() -> Vec<u8> {
    // Real yenc-encoded "Hello, yEnc!" with CRLF
    let data = b"r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n";
    decode_yenc_line(data)
}

library_benchmark_group!(
    name = yenc_benches;
    benchmarks = decode_various_sizes, decode_with_escapes, decode_real_world
);

main!(library_benchmark_groups = yenc_benches);
