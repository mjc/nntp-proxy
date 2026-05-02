//! Benchmarks for typed cache ingestion from response bytes.
//!
//! Run with: cargo bench --bench cache_ingest_semantic

use divan::{Bencher, black_box};
use nntp_proxy::cache::{ArticleCache, ArticleEntry};
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
                        black_box(ArticleEntry::from_response_bytes(black_box(
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

mod cache_upsert {
    use super::{
        ArticleCache, BackendId, Bencher, Duration, MessageId, article_response, black_box,
    };

    #[divan::bench(sample_count = 100, sample_size = 50)]
    fn response_bytes(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = ArticleCache::new(16 * 1024 * 1024, Duration::from_secs(300), true);
        let bytes = article_response(64 * 1024);

        bencher
            .counter(divan::counter::BytesCount::new(bytes.len()))
            .bench(|| {
                rt.block_on(async {
                    let msg_id = MessageId::from_borrowed("<bench@example.com>").unwrap();
                    cache
                        .upsert(
                            msg_id,
                            black_box(bytes.as_slice()),
                            BackendId::from_index(0),
                            0.into(),
                        )
                        .await;
                });
            });
    }
}
