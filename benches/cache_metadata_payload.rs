//! Mixed hybrid-cache workload for metadata and retained-payload updates.
//!
//! Run with: `cargo bench --bench cache_metadata_payload`

use divan::{Bencher, black_box};
use nntp_proxy::cache::{HybridCacheConfig, UnifiedCache};
use nntp_proxy::protocol::StatusCode;
use nntp_proxy::types::{BackendId, MessageId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tempfile::{TempDir, tempdir};

const ARTICLE_BODY: &str = "x";

fn main() {
    divan::main();
}

fn benchmark_cache() -> (tokio::runtime::Runtime, TempDir, UnifiedCache) {
    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime");
    let directory = tempdir().expect("benchmark cache directory");
    let config = HybridCacheConfig {
        memory_capacity: 4 * 1024 * 1024,
        disk_capacity: 64 * 1024 * 1024,
        disk_path: directory.path().to_path_buf(),
        ttl: Duration::from_secs(300),
        compression: nntp_proxy::config::CompressionCodec::None,
        shards: 16,
    };
    let cache = runtime
        .block_on(UnifiedCache::hybrid(config))
        .expect("hybrid cache");
    (runtime, directory, cache)
}

fn message_id(sequence: u64) -> MessageId<'static> {
    MessageId::new(format!("<bench-{sequence}@example.com>")).expect("benchmark message ID")
}

fn article_response(sequence: u64) -> Vec<u8> {
    format!(
        "220 42 <bench-{sequence}@example.com>\r\nSubject: Benchmark\r\n\r\n{ARTICLE_BODY}\r\n.\r\n"
    )
    .into_bytes()
}

#[divan::bench(sample_count = 20, sample_size = 1)]
fn metadata_only_updates(bencher: Bencher) {
    let (runtime, _directory, cache) = benchmark_cache();
    let sequence = AtomicU64::new(0);

    bencher.bench(|| {
        let id = message_id(sequence.fetch_add(1, Ordering::Relaxed));
        runtime.block_on(cache.record_backend_has_status(
            id,
            StatusCode::new(223),
            BackendId::from_index(0),
            0.into(),
        ));
    });

    runtime
        .block_on(cache.close())
        .expect("close benchmark cache");
}

#[divan::bench(sample_count = 20, sample_size = 1)]
fn retained_payload_updates(bencher: Bencher) {
    let (runtime, _directory, cache) = benchmark_cache();
    let sequence = AtomicU64::new(0);

    bencher.bench(|| {
        let sequence = sequence.fetch_add(1, Ordering::Relaxed);
        runtime.block_on(cache.upsert_ingest(
            message_id(sequence),
            black_box(article_response(sequence)),
            BackendId::from_index(0),
            0.into(),
        ));
    });

    runtime
        .block_on(cache.close())
        .expect("close benchmark cache");
}

#[divan::bench(sample_count = 20, sample_size = 1)]
fn mixed_metadata_and_payload_updates(bencher: Bencher) {
    let (runtime, _directory, cache) = benchmark_cache();
    let sequence = AtomicU64::new(0);

    bencher.bench(|| {
        let sequence = sequence.fetch_add(2, Ordering::Relaxed);
        runtime.block_on(async {
            cache
                .record_backend_has_status(
                    message_id(sequence),
                    StatusCode::new(223),
                    BackendId::from_index(0),
                    0.into(),
                )
                .await;
            cache
                .upsert_ingest(
                    message_id(sequence + 1),
                    black_box(article_response(sequence + 1)),
                    BackendId::from_index(0),
                    0.into(),
                )
                .await;
        });
    });

    runtime
        .block_on(cache.close())
        .expect("close benchmark cache");
}
