//! Benchmarks for backend selection and routing
//!
//! Measures hot-path router operations:
//! - `route` with weighted round-robin strategy
//! - `route` with least-loaded strategy
//! - availability-aware `route(RouteRequest::...)` (partially exhausted)
//! - `complete_command` throughput
//!
//! Run with: cargo bench --bench `router_selection`

use divan::{Bencher, black_box};
use nntp_proxy::config::BackendSelectionStrategy;
use nntp_proxy::pool::DeadpoolConnectionProvider;
use nntp_proxy::router::{BackendSelector, RouteRequest};
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
            ServerName::try_new(name).unwrap(),
            make_provider(10),
            u8::try_from(i / 2).expect("benchmark tier fits in u8"), // Tier 0 for first 2, tier 1 for next 2, etc.
        );
    }
    Arc::new(selector)
}

// =============================================================================
// Weighted Round-Robin Selection
// =============================================================================

mod weighted_round_robin {
    use super::{
        BackendSelectionStrategy, Bencher, ClientId, RouteRequest, black_box, make_router,
    };

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_2_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 2);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route(RouteRequest::new(black_box(client_id)))
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
                .route(RouteRequest::new(black_box(client_id)))
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
                .route(RouteRequest::new(black_box(client_id)))
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
    use super::{
        BackendSelectionStrategy, Bencher, ClientId, RouteRequest, black_box, make_router,
    };

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_2_backends(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::LeastLoaded, 2);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route(RouteRequest::new(black_box(client_id)))
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
                .route(RouteRequest::new(black_box(client_id)))
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
    use super::{
        BackendSelectionStrategy, Bencher, ClientId, RouteRequest, black_box, make_router,
    };
    use nntp_proxy::cache::ArticleAvailability;

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_no_exhaustion(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        let avail = ArticleAvailability::new(); // Fresh, nothing exhausted
        bencher.bench(|| {
            let backend = router
                .route(RouteRequest::new(black_box(client_id)).with_availability(black_box(&avail)))
                .unwrap();
            router.complete_command(backend.backend_id());
            black_box(backend)
        });
    }

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_half_exhausted(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        let mut avail = ArticleAvailability::new();
        // Mark first 2 backends as missing
        avail.record_missing_slot(nntp_proxy::cache::AvailabilitySlot::new(0).unwrap());
        avail.record_missing_slot(nntp_proxy::cache::AvailabilitySlot::new(1).unwrap());
        bencher.bench(|| {
            let backend = router
                .route(RouteRequest::new(black_box(client_id)).with_availability(black_box(&avail)))
                .unwrap();
            router.complete_command(backend.backend_id());
            black_box(backend)
        });
    }
}

// =============================================================================
// Availability-aware routing with least-loaded selection (the default)
// =============================================================================

mod availability_least_loaded {
    use super::{
        BackendSelectionStrategy, Bencher, ClientId, RouteRequest, black_box, make_router,
    };
    use nntp_proxy::cache::ArticleAvailability;

    #[divan::bench(sample_count = 1000, sample_size = 100)]
    fn route_no_exhaustion(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::LeastLoaded, 4);
        let client_id = ClientId::new();
        let avail = ArticleAvailability::new();
        bencher.bench(|| {
            let backend = router
                .route(RouteRequest::new(black_box(client_id)).with_availability(black_box(&avail)))
                .unwrap();
            router.complete_command(backend.backend_id());
            black_box(backend)
        });
    }
}

// =============================================================================
// Article standing: availability-aware routing weighted by standing
// =============================================================================

mod availability_standing_adaptive {
    use super::{
        Arc, BackendId, BackendSelectionStrategy, BackendSelector, Bencher, ClientId, RouteRequest,
        ServerName, black_box, make_provider,
    };
    use nntp_proxy::cache::ArticleAvailability;
    use nntp_proxy::pool::admission::{ArticleAdmission, ArticleLookupOutcome, ArticleStanding};
    use nntp_proxy::router::RouteLookup;

    fn make_adaptive_router(strategy: BackendSelectionStrategy) -> Arc<BackendSelector> {
        let mut selector = BackendSelector::with_strategy(strategy).with_article_admission(
            ArticleAdmission::default().with_standing(ArticleStanding::Adaptive),
        );
        for i in 0..4 {
            selector.add_backend(
                ServerName::try_new(format!("backend-{i}")).unwrap(),
                make_provider(10),
                u8::try_from(i / 2).expect("benchmark tier fits in u8"),
            );
        }
        // One demoted backend per tier, so the weights actually differ.
        for backend in [0, 2] {
            let provider = selector
                .backend_provider(BackendId::from_index(backend))
                .unwrap();
            for _ in 0..40 {
                provider.note_article_lookup(ArticleLookupOutcome::Missing);
            }
        }
        Arc::new(selector)
    }

    #[divan::bench(
        sample_count = 1000,
        sample_size = 100,
        args = [BackendSelectionStrategy::LeastLoaded, BackendSelectionStrategy::WeightedRoundRobin]
    )]
    fn route_no_exhaustion(bencher: Bencher, strategy: BackendSelectionStrategy) {
        let router = make_adaptive_router(strategy);
        let client_id = ClientId::new();
        let avail = ArticleAvailability::new();
        bencher.bench(|| {
            let backend = router
                .route(
                    RouteRequest::new(black_box(client_id))
                        .with_availability(black_box(&avail))
                        .for_lookup(RouteLookup::Article),
                )
                .unwrap();
            router.complete_command(backend.backend_id());
            black_box(backend)
        });
    }
}

// =============================================================================
// complete_command throughput
// =============================================================================

mod complete_command {
    use super::{BackendId, BackendSelectionStrategy, Bencher, black_box, make_router};

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
    use super::{
        BackendSelectionStrategy, Bencher, ClientId, RouteRequest, black_box, make_router,
    };

    #[divan::bench(sample_count = 100, sample_size = 100, threads = [1, 2, 4, 8])]
    fn route_and_complete(bencher: Bencher) {
        let router = make_router(BackendSelectionStrategy::WeightedRoundRobin, 4);
        let client_id = ClientId::new();
        bencher.bench(|| {
            let id = router
                .route(RouteRequest::new(black_box(client_id)))
                .unwrap();
            router.complete_command(id);
            black_box(id)
        });
    }
}
