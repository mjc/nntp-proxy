//! Comprehensive tests for ArticleAvailability bitset semantics and retry logic
//!
//! Tests cover:
//! - Bitset semantics (track missing backends, assume all have article by default)
//! - should_try() behavior
//! - all_missing() edge cases
//! - Integration with 430 retry loop
//! - Multi-backend exhaustion scenarios

use nntp_proxy::cache::ArticleAvailability;
use nntp_proxy::types::BackendId;

#[test]
fn test_fresh_availability_assumes_all_backends_have_article() {
    let avail = ArticleAvailability::new();

    // Fresh availability should allow trying all backends
    for i in 0..8 {
        assert!(
            avail.should_try(BackendId::from_index(i)),
            "Fresh availability should allow trying backend {}",
            i
        );
    }
}

#[test]
fn test_record_missing_prevents_retry() {
    let mut avail = ArticleAvailability::new();
    let backend = BackendId::from_index(2);

    // Initially should try
    assert!(avail.should_try(backend));

    // Record as missing (430 response)
    avail.record_missing(backend);

    // Should not try again
    assert!(!avail.should_try(backend));
}

#[test]
fn test_all_exhausted_with_single_backend() {
    let mut avail = ArticleAvailability::new();

    // Not all exhausted initially
    assert!(!avail.all_exhausted(1));

    // Record the only backend as missing
    avail.record_missing(BackendId::from_index(0));

    // Now all backends are exhausted
    assert!(avail.all_exhausted(1));
}

#[test]
fn test_all_exhausted_with_two_backends() {
    let mut avail = ArticleAvailability::new();

    assert!(!avail.all_exhausted(2));

    // Record first backend as missing
    avail.record_missing(BackendId::from_index(0));
    assert!(!avail.all_exhausted(2)); // Still have backend 1

    // Record second backend as missing
    avail.record_missing(BackendId::from_index(1));
    assert!(avail.all_exhausted(2)); // Now all exhausted
}

#[test]
fn test_all_exhausted_with_eight_backends() {
    let mut avail = ArticleAvailability::new();

    // Record 7 backends as missing
    for i in 0..7 {
        avail.record_missing(BackendId::from_index(i));
        assert!(
            !avail.all_exhausted(8),
            "Should not be all exhausted with backend 7 untried"
        );
    }

    // Record the last backend
    avail.record_missing(BackendId::from_index(7));
    assert!(
        avail.all_exhausted(8),
        "All 8 backends should be exhausted now"
    );
}

#[test]
fn test_partial_backend_subset() {
    let mut avail = ArticleAvailability::new();

    // Record backends 0, 2, 4 as missing
    avail.record_missing(BackendId::from_index(0));
    avail.record_missing(BackendId::from_index(2));
    avail.record_missing(BackendId::from_index(4));

    // Check should_try for each backend
    assert!(!avail.should_try(BackendId::from_index(0))); // missing
    assert!(avail.should_try(BackendId::from_index(1))); // untried
    assert!(!avail.should_try(BackendId::from_index(2))); // missing
    assert!(avail.should_try(BackendId::from_index(3))); // untried
    assert!(!avail.should_try(BackendId::from_index(4))); // missing
    assert!(avail.should_try(BackendId::from_index(5))); // untried

    // Not all exhausted (only tested 3 of 6 backends)
    assert!(!avail.all_exhausted(6));
}

#[test]
fn test_idempotent_record_missing() {
    let mut avail = ArticleAvailability::new();
    let backend = BackendId::from_index(3);

    // Record as missing multiple times
    avail.record_missing(backend);
    avail.record_missing(backend);
    avail.record_missing(backend);

    // Should still just be recorded once
    assert!(!avail.should_try(backend));

    // Bitset should still be correct
    assert_eq!(avail.as_u8(), 0b0000_1000);
}

#[test]
fn test_as_u8_bitset_encoding() {
    let mut avail = ArticleAvailability::new();

    // Initially all zeros (no backends missing)
    assert_eq!(avail.as_u8(), 0b0000_0000);

    // Record backend 0 as missing
    avail.record_missing(BackendId::from_index(0));
    assert_eq!(avail.as_u8(), 0b0000_0001);

    // Record backend 1 as missing
    avail.record_missing(BackendId::from_index(1));
    assert_eq!(avail.as_u8(), 0b0000_0011);

    // Record backend 7 as missing
    avail.record_missing(BackendId::from_index(7));
    assert_eq!(avail.as_u8(), 0b1000_0011);
}

#[test]
fn test_retry_loop_simulation_two_backends() {
    let mut avail = ArticleAvailability::new();
    let backend_count = 2;

    // Simulate retry loop
    let mut attempts = Vec::new();

    // Attempt 1: Try backend 0, gets 430
    let b0 = BackendId::from_index(0);
    assert!(avail.should_try(b0));
    attempts.push(b0);
    avail.record_missing(b0);
    assert!(!avail.all_exhausted(backend_count));

    // Attempt 2: Try backend 1, gets 430
    let b1 = BackendId::from_index(1);
    assert!(avail.should_try(b1));
    attempts.push(b1);
    avail.record_missing(b1);

    // Now all backends exhausted
    assert!(avail.all_exhausted(backend_count));

    // Verify we tried both backends
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], BackendId::from_index(0));
    assert_eq!(attempts[1], BackendId::from_index(1));
}

#[test]
fn test_retry_loop_simulation_finds_on_third() {
    let mut avail = ArticleAvailability::new();
    let _backend_count = 4;

    let mut attempts = Vec::new();

    // Backend 0: 430
    let b0 = BackendId::from_index(0);
    assert!(avail.should_try(b0));
    attempts.push(b0);
    avail.record_missing(b0);

    // Backend 1: 430
    let b1 = BackendId::from_index(1);
    assert!(avail.should_try(b1));
    attempts.push(b1);
    avail.record_missing(b1);

    // Backend 2: SUCCESS (220)
    let b2 = BackendId::from_index(2);
    assert!(avail.should_try(b2));
    attempts.push(b2);
    // Don't record missing - we got the article

    // Verify state
    assert!(!avail.all_exhausted(4));
    assert_eq!(attempts.len(), 3);

    // Verify backends 0 and 1 won't be tried again
    assert!(!avail.should_try(BackendId::from_index(0)));
    assert!(!avail.should_try(BackendId::from_index(1)));
    assert!(avail.should_try(BackendId::from_index(2)));
    assert!(avail.should_try(BackendId::from_index(3)));
}

#[test]
fn test_round_robin_skip_pattern() {
    let mut avail = ArticleAvailability::new();

    // Record backends 1 and 3 as missing
    avail.record_missing(BackendId::from_index(1));
    avail.record_missing(BackendId::from_index(3));

    // Simulate round-robin routing that needs to skip
    let sequence = vec![
        BackendId::from_index(0), // router gives 0 - should try
        BackendId::from_index(1), // router gives 1 - skip (missing)
        BackendId::from_index(2), // router gives 2 - should try
        BackendId::from_index(3), // router gives 3 - skip (missing)
        BackendId::from_index(0), // router gives 0 again - should try
    ];

    let mut tried = Vec::new();
    for backend in sequence {
        if avail.should_try(backend) {
            tried.push(backend);
        }
    }

    // Should have only tried backends 0 and 2 (skipped 1 and 3)
    assert_eq!(tried.len(), 3); // 0, 2, 0
    assert_eq!(tried[0], BackendId::from_index(0));
    assert_eq!(tried[1], BackendId::from_index(2));
    assert_eq!(tried[2], BackendId::from_index(0));
}

#[test]
fn test_all_exhausted_edge_case_zero_backends() {
    let avail = ArticleAvailability::new();

    // Edge case: zero backends means all are exhausted
    assert!(avail.all_exhausted(0));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Backend count 9 exceeds MAX_BACKENDS")]
fn test_all_exhausted_panics_with_nine_backends() {
    let mut avail = ArticleAvailability::new();

    // Record all 8 backends as missing
    for i in 0..8 {
        avail.record_missing(BackendId::from_index(i));
    }

    // This should panic in debug builds (config validation prevents this)
    avail.all_exhausted(9);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Backend index 8 exceeds MAX_BACKENDS")]
fn test_backend_id_out_of_range_panics() {
    let mut avail = ArticleAvailability::new();

    // Backend 8 is out of range - should panic in debug builds (config validation prevents this)
    avail.record_missing(BackendId::from_index(8));
}

#[test]
fn test_cache_persistence_scenario() {
    // Simulate: Article cached with availability info from previous request
    let mut avail = ArticleAvailability::new();

    // Previous request found that backends 1, 3, 5 don't have this article
    avail.record_missing(BackendId::from_index(1));
    avail.record_missing(BackendId::from_index(3));
    avail.record_missing(BackendId::from_index(5));

    // New request comes in - should skip backends 1, 3, 5
    assert!(avail.should_try(BackendId::from_index(0)));
    assert!(!avail.should_try(BackendId::from_index(1)));
    assert!(avail.should_try(BackendId::from_index(2)));
    assert!(!avail.should_try(BackendId::from_index(3)));
    assert!(avail.should_try(BackendId::from_index(4)));
    assert!(!avail.should_try(BackendId::from_index(5)));

    // Optimization: Skip directly to backends that might have it
    let potentially_available = (0..6)
        .map(BackendId::from_index)
        .filter(|&b| avail.should_try(b))
        .collect::<Vec<_>>();

    assert_eq!(
        potentially_available,
        vec![
            BackendId::from_index(0),
            BackendId::from_index(2),
            BackendId::from_index(4),
        ]
    );
}
