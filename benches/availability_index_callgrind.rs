//! Instruction costs of the same synchronous availability operations used by routing.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod benchmarks {
    use futures::FutureExt;
    use gungraun::{library_benchmark, library_benchmark_group, main};
    use nntp_proxy::cache::{AvailabilitySlot, CachedArticle, UnifiedCache};
    use nntp_proxy::types::MessageId;
    use std::hint::black_box;
    use std::time::Duration;

    fn empty() -> UnifiedCache {
        UnifiedCache::availability(Duration::MAX)
    }

    fn warm() -> UnifiedCache {
        let cache = empty();
        cache
            .record_availability_missing(
                MessageId::from_borrowed("<hit@example.com>").unwrap(),
                AvailabilitySlot::new(0).unwrap(),
            )
            .now_or_never()
            .unwrap();
        cache
    }

    #[library_benchmark(setup = warm)]
    fn hit(cache: UnifiedCache) -> (UnifiedCache, Option<CachedArticle>) {
        let id = MessageId::from_borrowed(black_box("<hit@example.com>")).unwrap();
        let result = black_box(cache.get(&id).now_or_never().unwrap());
        (cache, result)
    }

    #[library_benchmark(setup = warm)]
    fn miss(cache: UnifiedCache) -> (UnifiedCache, Option<CachedArticle>) {
        let id = MessageId::from_borrowed(black_box("<miss@example.com>")).unwrap();
        let result = black_box(cache.get(&id).now_or_never().unwrap());
        (cache, result)
    }

    #[library_benchmark(setup = empty)]
    fn cold_miss(cache: UnifiedCache) -> (UnifiedCache, Option<CachedArticle>) {
        let id = MessageId::from_borrowed(black_box("<miss@example.com>")).unwrap();
        let result = black_box(cache.get(&id).now_or_never().unwrap());
        (cache, result)
    }

    #[library_benchmark(setup = warm)]
    fn update(cache: UnifiedCache) -> UnifiedCache {
        let id = MessageId::from_borrowed(black_box("<hit@example.com>")).unwrap();
        cache
            .record_availability_missing(id, AvailabilitySlot::new(1).unwrap())
            .now_or_never()
            .unwrap();
        cache
    }

    library_benchmark_group!(name = availability; benchmarks = hit, miss, cold_miss, update);
    main!(library_benchmark_groups = availability);

    pub fn run() {
        main();
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() {
    benchmarks::run();
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("availability instruction benchmarks require a supported Linux target");
}
