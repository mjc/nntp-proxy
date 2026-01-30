//! Tests for racing precheck functionality
//!
//! Documents expected behavior of racing precheck:
//! - Returns first successful result immediately (~100ms vs 1700ms)
//! - Background task completes remaining queries
//! - Availability tracking updated correctly
//! - Pending count tracking during race
//!
//! Most functionality tested via integration tests with real proxy

#[test]
fn test_racing_precheck_performance_claims() {
    // Document performance characteristics of racing precheck

    // BEFORE (sequential or wait-all):
    // - Waited for slowest backend (1700ms)
    // - Unnecessary latency for clients
    // - Backend load imbalance (slow backend saturated)

    // AFTER (racing):
    // - Returns first success (~100ms typical)
    // - 17x latency improvement for cached checks
    // - Better load distribution (uses fast backends)
    // - Background task completes remaining for availability tracking

    // Tradeoffs:
    // + Much faster response for cached articles
    // + Better backend utilization
    // - Slightly more network traffic (queries all backends)
    // - Background tasks increase concurrency
}

#[test]
fn test_availability_bitset_semantics_for_retry() {
    // Document CRITICAL semantics of ArticleAvailability in retry loops

    // IMPORTANT: Don't create separate "tried backends" tracking!
    // ArticleAvailability already provides this via methods:
    //
    // - should_try(backend) == true  → Not yet tried or has article, ATTEMPT IT
    // - is_missing(backend) == true  → Tried and returned 430, SKIP IT
    //
    // Usage pattern in retry loop:
    // 1. Create fresh availability tracker for this request
    // 2. For each backend:
    //    a. Check if availability.is_missing(backend) → skip
    //    b. Try backend
    //    c. If 430: availability.record_missing(backend)
    //    d. Continue to next backend
    // 3. When all backends exhausted → send 430 to client

    // This is exactly the pattern racing precheck uses!
}
