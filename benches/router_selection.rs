//! Benchmarks for backend selection and routing
//!
//! Measures hot-path router operations:
//! - route_command with weighted round-robin strategy
//! - route_command with least-loaded strategy
//! - route_command_with_availability (partially exhausted)
//! - complete_command throughput
//!
//! Run with: cargo bench --bench router_selection

use divan::{Bencher, black_box};
use nntp_proxy::config::BackendSelectionStrategy;
use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::BackendSelector;
use nntp_proxy::types::{BackendId, ClientId, ServerName};
use std::sync::Arc;

fn main() {
    divan::main();
}

fn make_provider(max_conns: usize) -> DeadpoolConnectionProvider {
    DeadpoolConnectionProvider::new(
        "localhost".to_string(),
        119,
        "bench".to_string(),
        max_conns,
        None,
        None,
    )
}

fn make_router(strategy: BackendSelectionStrategy, num_backends: usize) -> Arc<BackendSelector> {
    let mut selector = BackendSelector::with_strategy(strategy);
    for i in 0..num_backends {
        let name = format!("backend-{i}");
        selector.add_backend(
            BackendId::from_index(i),
            ServerName::try_new(name).unwrap(),
            make_provider(10),
            (i / 2) as u8, // Tier 0 for first 2, tier 1 for next 2, etc.
        );
    }
    Arc::new(selector)
}

// =============================================================================
// Weighted Round-Robin Selection
// =============================================================================

mod weighted_round_robin {
    use super::*;

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_2_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 2);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route_command(black_box(client_id), black_box("ARTICLE"))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_4_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route_command(black_box(client_id), black_box("ARTICLE"))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_8_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 8);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route_command(black_box(client_id), black_box("ARTICLE"))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }
}

// =============================================================================
// Least-Loaded Selection
// =============================================================================

mod least_loaded {
    use super::*;

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_2_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::LeastLoaded, 2);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route_command(black_box(client_id), black_box("ARTICLE"))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_4_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::LeastLoaded, 4);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route_command(black_box(client_id), black_box("ARTICLE"))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }
}

// =============================================================================
// Availability-Aware Routing
// =============================================================================

mod availability_routing {
    use super::*;
    use nntp_proxy::cache::ArticleAvailability;

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_no_exhaustion(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        let avail = ArticleAvailability::new(); // Fresh, nothing exhausted
        bencher.bench(|| {
            let id = router
                .route_command_with_availability(
                    black_box(client_id),
                    black_box("ARTICLE"),
                    Some(black_box(&avail)),
                )
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_half_exhausted(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        let mut avail = ArticleAvailability::new();
        // Mark first 2 backends as missing
        avail.record_missing(BackendId::from_index(0));
        avail.record_missing(BackendId::from_index(1));
        bencher.bench(|| {
            let id = router
                .route_command_with_availability(
                    black_box(client_id),
                    black_box("ARTICLE"),
                    Some(black_box(&avail)),
                )
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }
}

// =============================================================================
// complete_command throughput
// =============================================================================

mod complete_command {
    use super::*;

    #[divan::bench(sample_count = 1000, sample_size = 1000)]
    fn complete_single(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let backend_id = BackendId::from_index(0);
        // Pre-increment so we have something to decrement
        router.mark_backend_pending(backend_id);
        bencher.bench(|| {
            router.mark_backend_pending(black_box(backend_id));
            router.complete_command(black_box(backend_id));
        });
    }
}

// =============================================================================
// Multi-threaded contention
// =============================================================================

mod contention {
    use super::*;

    #[divan::bench(sample_count = 100, sample_size = 100, threads = [1, 2, 4, 8])]
    fn route_and_complete(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route_command(black_box(client_id), black_box("ARTICLE"))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }
}
