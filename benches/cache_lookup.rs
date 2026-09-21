//! Benchmarks for article cache and availability tracking
//!
//! Measures performance of hot-path cache operations:
//! - `ArticleAvailability` bitset operations (`record_missing`, `should_try`, `all_exhausted`)
//! - `UnifiedCache` memory-tier get (hit vs miss)
//! - `UnifiedCache` upsert
//!
//! Run with: cargo bench --bench `cache_lookup`

use divan::{Bencher, black_box};
use nntp_proxy::cache::{ArticleAvailability, AvailabilityMask, AvailabilitySlot, UnifiedCache};
use nntp_proxy::types::{BackendId, MessageId};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    divan::main();
}

/// Contention experiment through the real availability cache. Multiple indexes
/// are a ceiling experiment: each retains the production capacity, so this is
/// not an equal-memory comparison or a proposed routing implementation.
mod index_contention {
    use super::*;
    use futures::FutureExt;
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::broadcast;

    static NEXT_WORKER: AtomicUsize = AtomicUsize::new(0);

    thread_local! {
        static CURSOR: Cell<usize> = const { Cell::new(0) };
        static WORKER: usize = NEXT_WORKER.fetch_add(1, Ordering::Relaxed);
    }

    #[derive(Clone, Copy)]
    enum Placement {
        ByArticle,
        ByWorker,
    }

    #[divan::bench(consts = [1, 16], args = [0, 10, 1], threads = [1, 2, 4, 8, 16], sample_count = 100, sample_size = 1000)]
    fn lookup_and_record<const INDEXES: usize>(bencher: Bencher, write_every: usize) {
        run::<INDEXES>(bencher, write_every, Placement::ByArticle);
    }

    /// Local-copy ceiling only: deliberately excludes replication work.
    #[divan::bench(args = [0, 10, 1], threads = [1, 2, 4, 8, 16], sample_count = 100, sample_size = 1000)]
    fn worker_local_without_replication(bencher: Bencher, write_every: usize) {
        run::<16>(bencher, write_every, Placement::ByWorker);
    }

    /// Equal block budget and the production fingerprint-to-shard route.
    #[divan::bench(consts = [1, 4, 8, 16, 32], args = [0, 10, 1], threads = [1, 2, 4, 8, 16], sample_count = 100, sample_size = 1000)]
    fn partitioned<const SHARDS: usize>(bencher: Bencher, write_every: usize) {
        let cache = UnifiedCache::availability_with_benchmark_shards(Duration::MAX, SHARDS);
        run_with_caches(bencher, write_every, Placement::ByArticle, &[cache]);
    }

    #[derive(Clone, Copy, Debug)]
    enum Workload {
        Empty,
        Hit,
        // Cold distributed misses against an empty index.
        Miss,
        // One absent key per corpus entry against a warm index; representative
        // distributed misses without a hot shard.
        DistributedMiss,
        // Adversarial contention controls, not representative Usenet traffic.
        HotKey,
        HotMiss,
        Skewed,
        Insert,
        Occupancy64K,
        Occupancy256K,
        LongIds,
        FiniteTtl,
    }

    #[divan::bench(consts = [1, 16, 32], args = [Workload::Empty, Workload::Hit, Workload::Miss, Workload::DistributedMiss, Workload::HotKey, Workload::HotMiss, Workload::Skewed, Workload::Insert, Workload::Occupancy64K, Workload::Occupancy256K, Workload::LongIds, Workload::FiniteTtl], threads = [1, 16], sample_count = 100, sample_size = 1000)]
    fn partition_workloads<const SHARDS: usize>(bencher: Bencher, workload: Workload) {
        let ttl = match workload {
            Workload::FiniteTtl => Duration::from_secs(300),
            _ => Duration::MAX,
        };
        let cache = UnifiedCache::availability_with_benchmark_shards(ttl, SHARDS);
        let count = match workload {
            Workload::Occupancy64K => 65_536,
            Workload::Insert | Workload::Occupancy256K => 262_144,
            _ => 1024,
        };
        let padding = match workload {
            Workload::LongIds => "x".repeat(200),
            _ => String::new(),
        };
        let ids: Vec<_> = (0..count)
            .map(|i| MessageId::new(format!("<load-{padding}{i}@example.com>")).unwrap())
            .collect();
        let miss_ids: Vec<_> = (0..count)
            .map(|i| MessageId::new(format!("<miss-{padding}{i}@example.com>")).unwrap())
            .collect();
        let slot = AvailabilitySlot::new(0).unwrap();
        match workload {
            Workload::Empty | Workload::Insert | Workload::Miss | Workload::HotMiss => {}
            _ => {
                for id in &ids {
                    cache
                        .record_availability_missing(id.clone(), slot)
                        .now_or_never()
                        .unwrap();
                }
            }
        }
        bencher.bench(|| {
            let cursor = CURSOR.with(|value| {
                let next = value.get().wrapping_add(1);
                value.set(next);
                next
            });
            let worker = WORKER.with(|worker| *worker % 16);
            let index = match workload {
                Workload::HotKey | Workload::HotMiss => 0,
                Workload::Skewed if !cursor.is_multiple_of(10) => 0,
                Workload::Insert => {
                    // Keep each worker on its own corpus partition. This
                    // measures distributed insertion streams rather than
                    // repeatedly racing over one shared set of keys.
                    let partition = ids.len() / 16;
                    worker * partition + cursor % partition
                }
                _ => cursor.wrapping_mul(17) % ids.len(),
            };
            match workload {
                Workload::Insert => cache
                    .record_availability_missing(ids[index].clone(), slot)
                    .now_or_never()
                    .unwrap(),
                Workload::Miss | Workload::DistributedMiss => {
                    black_box(cache.get(&miss_ids[index]).now_or_never().unwrap());
                }
                Workload::HotMiss => {
                    let absent = MessageId::from_borrowed("<absent@example.com>").unwrap();
                    black_box(cache.get(&absent).now_or_never().unwrap());
                }
                _ => {
                    black_box(cache.get(&ids[index]).now_or_never().unwrap());
                }
            }
        });
    }

    #[derive(Clone, Copy, Debug)]
    enum Maintenance {
        Save,
        Load,
        Metrics,
    }

    #[derive(Clone, Copy, Debug)]
    enum SnapshotActivity {
        Idle,
        ContinuousSave,
    }

    // Continuous saving is a cold-path interference stress test, not a model
    // of the application's snapshot frequency.
    #[divan::bench(consts = [1, 32], args = [SnapshotActivity::Idle, SnapshotActivity::ContinuousSave], threads = [16], sample_count = 100, sample_size = 1000)]
    fn snapshot_interference<const SHARDS: usize>(bencher: Bencher, activity: SnapshotActivity) {
        let cache = UnifiedCache::availability_with_benchmark_shards(Duration::MAX, SHARDS);
        let ids: Vec<_> = (0..1024)
            .map(|i| MessageId::new(format!("<snapshot-{i}@test>")).unwrap())
            .collect();
        for id in &ids {
            cache
                .record_availability_missing(id.clone(), AvailabilitySlot::new(0).unwrap())
                .now_or_never()
                .unwrap();
        }
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("availability.idx");
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let saver = match activity {
                SnapshotActivity::Idle => None,
                SnapshotActivity::ContinuousSave => Some(scope.spawn(|| {
                    let mut saves = 0;
                    while !stop.load(Ordering::Relaxed) {
                        cache.save_to_disk(&path).unwrap();
                        saves += 1;
                    }
                    saves
                })),
            };
            bencher.bench(|| {
                let index = CURSOR.with(|cursor| {
                    let index = cursor.get().wrapping_add(17) % ids.len();
                    cursor.set(index);
                    index
                });
                black_box(cache.get(&ids[index]).now_or_never().unwrap());
            });
            stop.store(true, Ordering::Relaxed);
            if let Some(saver) = saver {
                assert!(saver.join().unwrap() > 0);
            }
        });
    }

    #[divan::bench(consts = [1, 32], args = [Maintenance::Save, Maintenance::Load, Maintenance::Metrics], sample_count = 100, sample_size = 1)]
    fn partition_maintenance<const SHARDS: usize>(bencher: Bencher, operation: Maintenance) {
        let cache = UnifiedCache::availability_with_benchmark_shards(Duration::MAX, SHARDS);
        for i in 0..1024 {
            cache
                .record_availability_missing(
                    MessageId::new(format!("<save-{i}@test>")).unwrap(),
                    AvailabilitySlot::new(0).unwrap(),
                )
                .now_or_never()
                .unwrap();
        }
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("availability.idx");
        cache.save_to_disk(&path).unwrap();
        bencher.bench(|| match operation {
            Maintenance::Save => {
                black_box(cache.save_to_disk(&path).unwrap());
            }
            Maintenance::Load => {
                black_box(cache.load_from_disk(&path).unwrap());
            }
            Maintenance::Metrics => {
                black_box((cache.entry_count(), cache.hit_rate(), cache.weighted_size()));
            }
        });
    }

    #[divan::bench(consts = [1, 32], sample_count = 100, sample_size = 1)]
    fn expired_partition_lookup<const SHARDS: usize>(bencher: Bencher) {
        let id = MessageId::from_borrowed("<expired@test>").unwrap();
        bencher
            .with_inputs(|| {
                let cache = UnifiedCache::availability_with_benchmark_shards(
                    Duration::from_millis(2),
                    SHARDS,
                );
                cache
                    .record_availability_missing(id.clone(), AvailabilitySlot::new(0).unwrap())
                    .now_or_never()
                    .unwrap();
                std::thread::sleep(Duration::from_millis(5));
                cache
            })
            .bench_refs(|cache| {
                assert!(black_box(cache.get(&id).now_or_never().unwrap()).is_none());
            });
    }

    struct Replica {
        cache: UnifiedCache,
        updates: broadcast::Receiver<MessageId<'static>>,
        operations: usize,
    }

    impl Replica {
        fn apply_pending(&mut self, slot: AvailabilitySlot) {
            loop {
                match self.updates.try_recv() {
                    Ok(id) => self
                        .cache
                        .record_availability_missing(id, slot)
                        .now_or_never()
                        .expect("availability update is synchronous"),
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(error) => panic!("replication failed instead of converging: {error}"),
                }
            }
        }
    }

    /// Includes publication, queue draining, and applying remote updates. The
    /// bounded queue must not lose updates: lag is a failed experiment. Expiry
    /// is disabled, as in the other topology experiments. Worker ownership is
    /// simulated with uncontended locks; this is not a production TLS design.
    #[divan::bench(consts = [1, 64, 256], args = [0, 10, 1], threads = [1, 2, 4, 8, 16], sample_count = 100, sample_size = 1000)]
    fn worker_local_with_replication<const SYNC_EVERY: usize>(
        bencher: Bencher,
        write_every: usize,
    ) {
        let (updates, _) = broadcast::channel(65_536);
        let replicas: Vec<_> = (0..16)
            .map(|_| {
                Mutex::new(Replica {
                    cache: UnifiedCache::availability(Duration::MAX),
                    updates: updates.subscribe(),
                    operations: 0,
                })
            })
            .collect();
        let ids: Vec<_> = (0..1024)
            .map(|i| MessageId::new(format!("<contention-{i}@example.com>")).unwrap())
            .collect();
        let slot = AvailabilitySlot::new(0).unwrap();
        // Exercise convergence through the same queue/drain path before timing.
        assert!(
            replicas[0]
                .lock()
                .unwrap()
                .cache
                .get(&ids[0])
                .now_or_never()
                .unwrap()
                .is_none()
        );
        for id in &ids {
            updates.send(id.clone()).unwrap();
        }
        for replica in &replicas {
            let mut replica = replica.lock().unwrap();
            replica.apply_pending(slot);
            for id in &ids {
                assert!(replica.cache.get(id).now_or_never().unwrap().is_some());
            }
        }
        bencher.bench(|| {
            let worker = WORKER.with(|worker| worker % replicas.len());
            let mut replica = replicas[worker].lock().unwrap();
            replica.operations = replica.operations.wrapping_add(1);
            let cursor = replica.operations;
            if cursor.is_multiple_of(SYNC_EVERY) {
                replica.apply_pending(slot);
            }
            let id = &ids[cursor.wrapping_mul(17) % ids.len()];
            if write_every != 0 && cursor.is_multiple_of(write_every) {
                replica
                    .cache
                    .record_availability_missing(id.clone(), slot)
                    .now_or_never()
                    .expect("availability update is synchronous");
                updates
                    .send(id.clone())
                    .expect("replicas remain subscribed");
            } else {
                black_box(
                    replica
                        .cache
                        .get(black_box(id))
                        .now_or_never()
                        .expect("availability lookup is synchronous"),
                );
            }
        });
        // Finish queued work for participating workers and detect lag even when
        // it occurred after their last scheduled drain.
        for replica in &replicas {
            let mut replica = replica.lock().unwrap();
            if replica.operations != 0 {
                replica.apply_pending(slot);
            }
        }
    }

    fn run<const INDEXES: usize>(bencher: Bencher, write_every: usize, placement: Placement) {
        let caches: Vec<_> = (0..INDEXES)
            .map(|_| UnifiedCache::availability(Duration::MAX))
            .collect();
        run_with_caches(bencher, write_every, placement, &caches);
    }

    fn run_with_caches(
        bencher: Bencher,
        write_every: usize,
        placement: Placement,
        caches: &[UnifiedCache],
    ) {
        let ids: Vec<_> = (0..1024)
            .map(|i| MessageId::new(format!("<contention-{i}@example.com>")).unwrap())
            .collect();
        let slot = AvailabilitySlot::new(0).unwrap();
        for cache in caches {
            for id in &ids {
                cache
                    .record_availability_missing(id.clone(), slot)
                    .now_or_never()
                    .expect("availability update is synchronous");
            }
        }
        bencher.bench(|| {
            let cursor = CURSOR.with(|value| {
                let next = value.get().wrapping_add(1);
                value.set(next);
                next
            });
            let i = cursor.wrapping_mul(17) % ids.len();
            let index = match placement {
                Placement::ByArticle => i % caches.len(),
                Placement::ByWorker => WORKER.with(|worker| worker % caches.len()),
            };
            let cache = &caches[index];
            if write_every != 0 && cursor.is_multiple_of(write_every) {
                cache
                    .record_availability_missing(ids[i].clone(), slot)
                    .now_or_never()
                    .expect("availability update is synchronous");
            } else {
                black_box(
                    cache
                        .get(black_box(&ids[i]))
                        .now_or_never()
                        .expect("availability lookup is synchronous"),
                );
            }
        });
    }
}

// =============================================================================
// ArticleAvailability bitset operations
// =============================================================================

mod availability {
    use super::{ArticleAvailability, AvailabilityMask, AvailabilitySlot, Bencher, black_box};

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn record_missing(bencher: Bencher) {
        bencher.bench(|| {
            let mut avail = ArticleAvailability::new();
            for i in 0..8u8 {
                avail.record_missing_slot(AvailabilitySlot::new(i as usize).unwrap());
            }
            black_box(avail)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn is_missing_all_available(bencher: Bencher) {
        let avail = ArticleAvailability::new();
        bencher.bench(|| {
            let mut result = true;
            for i in 0..8u8 {
                result &=
                    !black_box(&avail).is_missing_slot(AvailabilitySlot::new(i as usize).unwrap());
            }
            black_box(result)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn is_missing_partially_exhausted(bencher: Bencher) {
        let mut avail = ArticleAvailability::new();
        // Mark backends 0-3 as missing (half exhausted)
        for i in 0..4u8 {
            avail.record_missing_slot(AvailabilitySlot::new(i as usize).unwrap());
        }
        bencher.bench(|| {
            let mut result = true;
            for i in 0..8u8 {
                result &=
                    !black_box(&avail).is_missing_slot(AvailabilitySlot::new(i as usize).unwrap());
            }
            black_box(result)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn all_exhausted_not_yet(bencher: Bencher) {
        let mut avail = ArticleAvailability::new();
        // Mark 3 of 4 as missing
        for i in 0..3u8 {
            avail
                .record_missing_slot(nntp_proxy::cache::AvailabilitySlot::new(i as usize).unwrap());
        }
        let configured = AvailabilityMask::from_slots(
            &(0..4)
                .map(|index| AvailabilitySlot::new(index).unwrap())
                .collect::<Vec<_>>(),
        );
        bencher.bench(|| black_box(black_box(&avail).all_exhausted(black_box(configured))));
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn all_exhausted_yes(bencher: Bencher) {
        let mut avail = ArticleAvailability::new();
        for i in 0..4u8 {
            avail
                .record_missing_slot(nntp_proxy::cache::AvailabilitySlot::new(i as usize).unwrap());
        }
        let configured = AvailabilityMask::from_slots(
            &(0..4)
                .map(|index| AvailabilitySlot::new(index).unwrap())
                .collect::<Vec<_>>(),
        );
        bencher.bench(|| black_box(black_box(&avail).all_exhausted(black_box(configured))));
    }
}

// =============================================================================
// UnifiedCache memory tier operations
// =============================================================================

mod unified_cache {
    use super::{
        Arc, AvailabilitySlot, BackendId, Bencher, Duration, MessageId, UnifiedCache, black_box,
    };
    use nntp_proxy::protocol::RequestKind;
    use nntp_proxy::session::benchmark_framed_cache_response;

    fn make_cache() -> Arc<UnifiedCache> {
        Arc::new(UnifiedCache::memory(
            1024 * 1024, // 1MB capacity
            Duration::from_secs(300),
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
            let response = b"220 0 <hit@example.com>\r\nSubject: test\r\n\r\nbody\r\n.\r\n";
            cache
                .upsert_framed_ingest(
                    msg_id.to_owned(),
                    benchmark_framed_cache_response(response, RequestKind::Article, 4096),
                    BackendId::from_index(0),
                    0.into(),
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
            .with_inputs(|| {
                (
                    MessageId::from_borrowed("<bench@test.com>")
                        .unwrap()
                        .to_owned(),
                    benchmark_framed_cache_response(&data, RequestKind::Article, 4096),
                )
            })
            .bench_values(|(msg_id, framed)| {
                rt.block_on(async {
                    cache
                        .upsert_framed_ingest(
                            msg_id,
                            black_box(framed),
                            BackendId::from_index(0),
                            0.into(),
                        )
                        .await;
                });
            });
    }

    #[divan::bench(sample_count = 100, sample_size = 100)]
    fn record_backend_missing(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();

        bencher.bench(|| {
            rt.block_on(async {
                let msg_id = MessageId::from_borrowed("<missing@test.com>").unwrap();
                cache
                    .record_availability_missing(
                        msg_id.to_owned(),
                        AvailabilitySlot::new(0).unwrap(),
                    )
                    .await;
            });
        });
    }
}

mod availability_cache {
    use super::{Arc, AvailabilitySlot, Bencher, MessageId, UnifiedCache, black_box};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn make_cache() -> Arc<UnifiedCache> {
        Arc::new(UnifiedCache::availability(std::time::Duration::MAX))
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
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

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn get_hit(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();
        let msg_id = MessageId::from_borrowed("<hit@example.com>").unwrap();
        rt.block_on(async {
            cache
                .record_availability_missing(msg_id.clone(), AvailabilitySlot::new(0).unwrap())
                .await;
        });

        bencher.bench(|| rt.block_on(async { black_box(cache.get(&msg_id).await) }));
    }

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn record_missing(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = make_cache();
        let next_id = AtomicU64::new(0);

        bencher.bench(|| {
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let msg_id = MessageId::new(format!("<bench-{id}@example.com>")).unwrap();
            rt.block_on(async {
                cache
                    .record_availability_missing(msg_id, AvailabilitySlot::new(0).unwrap())
                    .await;
                black_box(());
            });
        });
    }
}
