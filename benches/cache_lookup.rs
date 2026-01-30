//! Benchmarks for article cache and availability tracking
//!
//! Measures performance of hot-path cache operations:
//! - ArticleAvailability bitset operations (record_missing, should_try, all_exhausted)
//! - UnifiedCache memory-tier get (hit vs miss)
//! - UnifiedCache upsert
//!
//! Run with: cargo bench --bench cache_lookup

use divan::{Bencher, black_box};
use nntp_proxy::cache::{ArticleAvailability, UnifiedCache};
use nntp_proxy::router::BackendCount;
use nntp_proxy::types::{BackendId, MessageId};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    divan::main();
}

// =============================================================================
// ArticleAvailability bitset operations
// =============================================================================

mod availability {
    use super::*;

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn record_missing(bencher: Bencher) {
        bencher.bench(|| {
            let mut avail = ArticleAvailability::new();
            for i in 0..8u8 {
                avail.record_missing(BackendId::from_index(i as usize));
            }
            black_box(avail)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn should_try_all_available(bencher: Bencher) {
        let avail = ArticleAvailability::new();
        bencher.bench(|| {
            let mut result = true;
            for i in 0..8u8 {
                result &= black_box(&avail).should_try(BackendId::from_index(i as usize));
            }
            black_box(result)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn should_try_partially_exhausted(bencher: Bencher) {
        let mut avail = ArticleAvailability::new();
        // Mark backends 0-3 as missing (half exhausted)
        for i in 0..4u8 {
            avail.record_missing(BackendId::from_index(i as usize));
        }
        bencher.bench(|| {
            let mut result = true;
            for i in 0..8u8 {
                result &= black_box(&avail).should_try(BackendId::from_index(i as usize));
            }
            black_box(result)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn all_exhausted_not_yet(bencher: Bencher) {
        let mut avail = ArticleAvailability::new();
        // Mark 3 of 4 as missing
        for i in 0..3u8 {
            avail.record_missing(BackendId::from_index(i as usize));
        }
        let count = BackendCount::new(4);
        bencher.bench(|| black_box(black_box(&avail).all_exhausted(black_box(count))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn all_exhausted_yes(bencher: Bencher) {
        let mut avail = ArticleAvailability::new();
        for i in 0..4u8 {
            avail.record_missing(BackendId::from_index(i as usize));
        }
        let count = BackendCount::new(4);
        bencher.bench(|| black_box(black_box(&avail).all_exhausted(black_box(count))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn record_has(bencher: Bencher) {
        bencher.bench(|| {
            let mut avail = ArticleAvailability::new();
            for i in 0..4u8 {
                avail.record_has(BackendId::from_index(i as usize));
            }
            black_box(avail)
        });
    }
}

// =============================================================================
// UnifiedCache memory tier operations
// =============================================================================

mod unified_cache {
    use super::*;

    fn make_cache() -> Arc<UnifiedCache> {
        Arc::new(UnifiedCache::memory(
            1024 * 1024, // 1MB capacity
            Duration::from_secs(300),
            true,
        ))
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn get_miss(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();
        bencher.bench(|| {
            rt.block_on(async {
                let msg_id = MessageId::from_borrowed("<miss@example.com>").unwrap();
                black_box(cache.get(&msg_id).await)
            })
        });
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn get_hit(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();
        // Pre-populate
        rt.block_on(async {
            let msg_id = MessageId::from_borrowed("<hit@example.com>").unwrap();
            cache
                .upsert(
                    msg_id.to_owned(),
                    b"220 0 <hit@example.com>\r\nSubject: test\r\n\r\nbody\r\n.\r\n".to_vec(),
                    BackendId::from_index(0),
                    0,
                )
                .await;
        });
        bencher.bench(|| {
            rt.block_on(async {
                let msg_id = MessageId::from_borrowed("<hit@example.com>").unwrap();
                black_box(cache.get(&msg_id).await)
            })
        });
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn upsert(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();
        let data =
            b"220 0 <bench@test.com>\r\nSubject: bench\r\n\r\nbenchmark body\r\n.\r\n".to_vec();
        bencher
            .counter(divan::counter::BytesCount::new(data.len()))
            .bench(|| {
                rt.block_on(async {
                    let msg_id = MessageId::from_borrowed("<bench@test.com>").unwrap();
                    cache
                        .upsert(msg_id.to_owned(), data.clone(), BackendId::from_index(0), 0)
                        .await;
                })
            });
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn sync_availability(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();
        let mut avail = ArticleAvailability::new();
        avail.record_missing(BackendId::from_index(0));
        avail.record_has(BackendId::from_index(1));

        bencher.bench(|| {
            rt.block_on(async {
                let msg_id = MessageId::from_borrowed("<sync@test.com>").unwrap();
                cache.sync_availability(msg_id.to_owned(), &avail).await;
            })
        });
    }
}
