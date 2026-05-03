//! Benchmarks for generating NNTP responses from typed cache entries.
//!
//! These benches render typed responses through the same writer path used by
//! cache hits. They cover ARTICLE-derived HEAD/BODY/STAT responses as well as
//! missing and availability-only entries that should produce no payload.
//!
//! Run with: cargo bench --bench cache_response_generation

use divan::{Bencher, black_box};
use futures::executor::block_on;
use nntp_proxy::cache::{ArticleCache, ArticleEntry};
use nntp_proxy::protocol::RequestKind;
use nntp_proxy::types::{BackendId, MessageId};
use std::time::Duration;

fn main() {
    divan::main();
}

const MSG_ID: &str = "<bench@example.com>";
const ARTICLE_RESPONSE: &[u8] = b"220 42 <bench@example.com>\r\n\
Subject: Benchmark\r\n\
From: bench@example.com\r\n\
Message-ID: <bench@example.com>\r\n\
\r\n\
This is the benchmark body.\r\n\
It has multiple lines.\r\n\
.\r\n";

fn article_entry() -> ArticleEntry {
    cache_entry_from_bytes(ARTICLE_RESPONSE)
}

fn cache_entry_from_bytes(response: impl AsRef<[u8]>) -> ArticleEntry {
    let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true);
    let msg_id = MessageId::from_borrowed(MSG_ID).unwrap();

    block_on(async {
        cache
            .upsert_response(
                msg_id.clone(),
                response.as_ref(),
                BackendId::from_index(0),
                0.into(),
            )
            .await;
        cache.get(&msg_id).await.expect("bench entry cached")
    })
}

fn write_cached_response(entry: &ArticleEntry, request_kind: RequestKind) -> usize {
    entry
        .response_for(request_kind, MSG_ID)
        .map(|response| {
            let mut sink = tokio::io::sink();
            block_on(response.write_to(&mut sink)).unwrap();
            response.wire_len().get()
        })
        .unwrap_or_default()
}

mod article_derived_hits {
    use super::{Bencher, RequestKind, article_entry, black_box, write_cached_response};

    macro_rules! bench_cached_response {
        ($name:ident, $request_kind:expr) => {
            #[divan::bench(sample_count = 1000, sample_size = 1000)]
            fn $name(bencher: Bencher) {
                let entry = article_entry();

                bencher.bench(|| {
                    black_box(write_cached_response(
                        black_box(&entry),
                        black_box($request_kind),
                    ))
                });
            }
        };
    }

    bench_cached_response!(article_from_article, RequestKind::Article);
    bench_cached_response!(head_from_article, RequestKind::Head);
    bench_cached_response!(body_from_article, RequestKind::Body);
    bench_cached_response!(stat_from_article, RequestKind::Stat);
}

mod no_payload_entries {
    use super::{Bencher, RequestKind, black_box, cache_entry_from_bytes, write_cached_response};

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn missing_entry_returns_none(bencher: Bencher) {
        let entry = cache_entry_from_bytes(b"430 No article\r\n");

        bencher.bench(|| {
            black_box(write_cached_response(
                black_box(&entry),
                black_box(RequestKind::Article),
            ))
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn availability_only_returns_none(bencher: Bencher) {
        let entry = cache_entry_from_bytes(b"220 42 <bench@example.com>\r\n");

        bencher.bench(|| {
            black_box(write_cached_response(
                black_box(&entry),
                black_box(RequestKind::Article),
            ))
        });
    }
}

mod near_limit_status_line {
    use super::{Bencher, RequestKind, black_box, block_on, cache_entry_from_bytes};

    const LONG_MSG_ID: &str = "<aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb@example.com>";

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn stat_with_long_message_id(bencher: Bencher) {
        let entry =
            cache_entry_from_bytes(b"220 42 <x@y>\r\nSubject: Benchmark\r\n\r\nBody\r\n.\r\n");

        bencher.bench(|| {
            black_box(
                entry
                    .response_for(black_box(RequestKind::Stat), black_box(LONG_MSG_ID))
                    .map(|response| {
                        let mut sink = tokio::io::sink();
                        block_on(response.write_to(&mut sink)).unwrap();
                        response.wire_len().get()
                    }),
            )
        });
    }
}
