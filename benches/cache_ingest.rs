//! Benchmarks for cache ingestion through the production framer boundary.
//!
//! Run with: `cargo bench --bench cache_ingest --features framing-bench`

use divan::{Bencher, black_box};
use nntp_proxy::cache::ArticleCache;
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::session::benchmark_framed_cache_response;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

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

fn bench_ingest(bencher: Bencher, response: Vec<u8>, kind: RequestKind) {
    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime");
    let cache = ArticleCache::new(16 * 1024 * 1024, Duration::from_secs(300));

    bencher
        .counter(divan::counter::BytesCount::new(response.len()))
        .with_inputs(|| benchmark_framed_cache_response(&response, kind, 4096))
        .bench_values(|framed| {
            runtime.block_on(async {
                let message_id = MessageId::from_borrowed("<bench@example.com>").unwrap();
                cache
                    .upsert_framed_ingest(
                        message_id,
                        black_box(framed),
                        BackendId::from_index(0),
                        0.into(),
                    )
                    .await;
            });
        });
}

mod ingest {
    use super::{
        BODY_RESPONSE, Bencher, HEAD_RESPONSE, MISSING_RESPONSE, RequestKind, STAT_RESPONSE,
        article_response, bench_ingest,
    };

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn article_small_body(bencher: Bencher) {
        bench_ingest(bencher, article_response(128), RequestKind::Article);
    }

    #[divan::bench(sample_count = 200, sample_size = 100)]
    fn article_64k_body(bencher: Bencher) {
        bench_ingest(bencher, article_response(64 * 1024), RequestKind::Article);
    }

    #[divan::bench(sample_count = 50, sample_size = 100)]
    fn article_1mb_body(bencher: Bencher) {
        bench_ingest(bencher, article_response(1024 * 1024), RequestKind::Article);
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn head_only(bencher: Bencher) {
        bench_ingest(bencher, HEAD_RESPONSE.to_vec(), RequestKind::Head);
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn body_only(bencher: Bencher) {
        bench_ingest(bencher, BODY_RESPONSE.to_vec(), RequestKind::Body);
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn stat_only(bencher: Bencher) {
        bench_ingest(bencher, STAT_RESPONSE.to_vec(), RequestKind::Stat);
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn missing_430(bencher: Bencher) {
        bench_ingest(bencher, MISSING_RESPONSE.to_vec(), RequestKind::Article);
    }
}
