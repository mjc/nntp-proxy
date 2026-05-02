//! Benchmarks for generating NNTP wire responses from typed cache entries.
//!
//! These benches use `copy_to_slice` to keep the measured hit path free of heap
//! allocation. They cover ARTICLE-derived HEAD/BODY/STAT responses as well as
//! missing and availability-only entries that should produce no payload.
//!
//! Run with: cargo bench --bench cache_response_generation

use divan::{Bencher, black_box};
use nntp_proxy::cache::{ArticleEntry, CacheableStatusCode};
use nntp_proxy::protocol::StatusCode;

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
    ArticleEntry::from_backend_response(ARTICLE_RESPONSE)
}

fn copy_cached_response(entry: &ArticleEntry, verb: &[u8], out: &mut [u8]) -> usize {
    entry
        .response_parts_for_command_bytes(verb, MSG_ID)
        .and_then(|response| response.copy_to_slice(out))
        .unwrap_or_default()
}

fn alloc_cached_response(entry: &ArticleEntry, verb: &[u8]) -> usize {
    entry
        .response_parts_for_command_bytes(verb, MSG_ID)
        .map(|response| response.to_vec().len())
        .unwrap_or_default()
}

mod article_derived_hits {
    use super::{Bencher, article_entry, black_box, copy_cached_response};

    macro_rules! bench_cached_response {
        ($name:ident, $verb:literal) => {
            #[divan::bench(sample_count = 1000, sample_size = 1000)]
            fn $name(bencher: Bencher) {
                let entry = article_entry();

                bencher.bench(|| {
                    let mut out = [0u8; 2048];
                    black_box(copy_cached_response(
                        black_box(&entry),
                        black_box($verb),
                        black_box(&mut out),
                    ))
                });
            }
        };
    }

    bench_cached_response!(article_from_article, b"ARTICLE");
    bench_cached_response!(head_from_article, b"HEAD");
    bench_cached_response!(body_from_article, b"BODY");
    bench_cached_response!(stat_from_article, b"STAT");
}

mod allocating_baseline {
    use super::{Bencher, alloc_cached_response, article_entry, black_box};

    macro_rules! bench_cached_response_alloc {
        ($name:ident, $verb:literal) => {
            #[divan::bench(sample_count = 1000, sample_size = 1000)]
            fn $name(bencher: Bencher) {
                let entry = article_entry();

                bencher.bench(|| {
                    black_box(alloc_cached_response(black_box(&entry), black_box($verb)))
                });
            }
        };
    }

    bench_cached_response_alloc!(article_from_article_to_vec, b"ARTICLE");
    bench_cached_response_alloc!(head_from_article_to_vec, b"HEAD");
    bench_cached_response_alloc!(body_from_article_to_vec, b"BODY");
    bench_cached_response_alloc!(stat_from_article_to_vec, b"STAT");
}

mod no_payload_entries {
    use super::{
        ArticleEntry, Bencher, CacheableStatusCode, StatusCode, black_box, copy_cached_response,
    };

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn missing_entry_returns_none(bencher: Bencher) {
        let entry = ArticleEntry::from_backend_response(b"430 No article\r\n");

        bencher.bench(|| {
            let mut out = [0u8; 256];
            black_box(copy_cached_response(
                black_box(&entry),
                black_box(b"ARTICLE"),
                black_box(&mut out),
            ))
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn availability_only_returns_none(bencher: Bencher) {
        let entry = ArticleEntry::availability_only(
            StatusCode::new(CacheableStatusCode::Article.as_u16()),
            0.into(),
        );

        bencher.bench(|| {
            let mut out = [0u8; 256];
            black_box(copy_cached_response(
                black_box(&entry),
                black_box(b"ARTICLE"),
                black_box(&mut out),
            ))
        });
    }
}

mod near_limit_status_line {
    use super::{ArticleEntry, Bencher, black_box};

    const LONG_MSG_ID: &str = "<aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb@example.com>";

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn stat_with_long_message_id(bencher: Bencher) {
        let entry = ArticleEntry::from_backend_response(
            b"220 42 <x@y>\r\nSubject: Benchmark\r\n\r\nBody\r\n.\r\n",
        );

        bencher.bench(|| {
            let mut out = [0u8; 512];
            black_box(
                entry
                    .response_parts_for_command_bytes(black_box(b"STAT"), black_box(LONG_MSG_ID))
                    .and_then(|response| response.copy_to_slice(black_box(&mut out))),
            )
        });
    }
}
