//! Benchmarks for typed cache ingestion and cache buffer metadata reads.
//!
//! Public cache ingestion currently accepts contiguous backend responses for
//! semantic parsing; cache buffer status benchmarks cover the owned forms used
//! across async cache handoff, including chunked responses without flattening.
//!
//! Run with: cargo bench --bench cache_ingest_semantic

use divan::{Bencher, black_box};
use nntp_proxy::cache::{ArticleEntry, CacheBuffer};
use nntp_proxy::pool::{BufferPool, ChunkedResponse};
use nntp_proxy::types::BufferSize;
use smallvec::SmallVec;

fn main() {
    divan::main();
}

fn article_response(body_len: usize) -> Vec<u8> {
    let body = "x".repeat(body_len);
    format!(
        "220 42 <bench@example.com>\r\nSubject: Benchmark\r\nFrom: bench@example.com\r\n\r\n{body}\r\n.\r\n"
    )
    .into_bytes()
}

const HEAD_RESPONSE: &[u8] =
    b"221 42 <bench@example.com>\r\nSubject: Benchmark\r\nFrom: bench@example.com\r\n.\r\n";
const BODY_RESPONSE: &[u8] = b"222 42 <bench@example.com>\r\nBody line\r\n.\r\n";
const STAT_RESPONSE: &[u8] = b"223 42 <bench@example.com>\r\n";
const MISSING_RESPONSE: &[u8] = b"430 No article\r\n";

fn chunked_response(chunks: &[&[u8]]) -> ChunkedResponse {
    let pool = BufferPool::new(BufferSize::try_new(64).unwrap(), 8);
    chunks
        .iter()
        .fold(ChunkedResponse::default(), |mut response, chunk| {
            response.extend_from_slice(&pool, chunk);
            response
        })
}

mod semantic_ingest {
    use super::{
        ArticleEntry, BODY_RESPONSE, Bencher, HEAD_RESPONSE, MISSING_RESPONSE, STAT_RESPONSE,
        article_response, black_box,
    };

    macro_rules! bench_ingest {
        ($name:ident, $bytes:expr, $samples:expr) => {
            #[divan::bench(sample_count = $samples, sample_size = 100)]
            fn $name(bencher: Bencher) {
                let bytes = $bytes;
                bencher
                    .counter(divan::counter::BytesCount::new(bytes.len()))
                    .bench(|| {
                        black_box(ArticleEntry::from_backend_response(black_box(
                            bytes.as_slice(),
                        )))
                    });
            }
        };
    }

    bench_ingest!(article_small_body, article_response(128), 1000);
    bench_ingest!(article_64k_body, article_response(64 * 1024), 200);
    bench_ingest!(article_1mb_body, article_response(1024 * 1024), 50);
    bench_ingest!(head_only, HEAD_RESPONSE.to_vec(), 1000);
    bench_ingest!(body_only, BODY_RESPONSE.to_vec(), 1000);
    bench_ingest!(stat_only, STAT_RESPONSE.to_vec(), 1000);
    bench_ingest!(missing_430, MISSING_RESPONSE.to_vec(), 1000);
}

mod cache_buffer_status {
    use super::{CacheBuffer, SmallVec, black_box, chunked_response};
    use divan::Bencher;

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn vec_status_code(bencher: Bencher) {
        let buffer = CacheBuffer::Vec(b"220 42 <bench@example.com>\r\nBody\r\n.\r\n".to_vec());
        bencher.bench(|| black_box(black_box(&buffer).status_code()));
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn small_status_code(bencher: Bencher) {
        let buffer = CacheBuffer::Small(SmallVec::from_slice(b"223 42 <bench@example.com>\r\n"));
        bencher.bench(|| black_box(black_box(&buffer).status_code()));
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn chunked_status_code_split(bencher: Bencher) {
        let response =
            chunked_response(&[b"2", b"20", b" 42 <bench@example.com>\r\nBody\r\n.\r\n"]);
        let buffer = CacheBuffer::Chunked(response);
        bencher.bench(|| black_box(black_box(&buffer).status_code()));
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn chunked_flatten_baseline(bencher: Bencher) {
        let chunks = [
            b"220 42 <bench@example.com>\r\n".as_slice(),
            b"Body\r\n.\r\n",
        ];
        bencher.bench(|| {
            let response = chunked_response(&chunks);
            black_box(CacheBuffer::Chunked(response).into_vec())
        });
    }
}
