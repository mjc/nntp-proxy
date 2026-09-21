//! Keep allocation instrumentation separate from the wall-time comparison.

use divan::{Bencher, black_box};
use futures::FutureExt;
use nntp_proxy::cache::{AvailabilitySlot, UnifiedCache};
use nntp_proxy::types::MessageId;
use std::time::Duration;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Hit,
    Miss,
    Update,
}

#[divan::bench(consts = [1, 32], sample_count = 100, sample_size = 1)]
fn construct<const SHARDS: usize>(bencher: Bencher) {
    bencher.bench(|| UnifiedCache::availability_with_benchmark_shards(Duration::MAX, SHARDS));
}

#[divan::bench(consts = [1, 32], args = [Operation::Hit, Operation::Miss, Operation::Update], sample_count = 100, sample_size = 1000)]
fn operation<const SHARDS: usize>(bencher: Bencher, operation: Operation) {
    let cache = UnifiedCache::availability_with_benchmark_shards(Duration::MAX, SHARDS);
    let hit = MessageId::from_borrowed("<hit@test>").unwrap();
    let miss = MessageId::from_borrowed("<miss@test>").unwrap();
    let slot = AvailabilitySlot::new(0).unwrap();
    cache
        .record_availability_missing(hit.clone(), slot)
        .now_or_never()
        .unwrap();
    bencher.bench(|| match operation {
        Operation::Hit => {
            black_box(cache.get(&hit).now_or_never().unwrap());
        }
        Operation::Miss => {
            black_box(cache.get(&miss).now_or_never().unwrap());
        }
        Operation::Update => {
            cache
                .record_availability_missing(hit.clone(), slot)
                .now_or_never()
                .unwrap();
        }
    });
}
