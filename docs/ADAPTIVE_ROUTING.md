# Adaptive Routing and Tiered Backend Strategy

**Status:** Partial Implementation  
**Version:** 0.2.0  
**Last Updated:** 2024-11-26

---

## Table of Contents

1. [Overview](#overview)
2. [Goals](#goals)
3. [Background: SABnzbd's Approach](#background-sabnzbds-approach)
4. [Architecture](#architecture)
5. [Routing Strategies](#routing-strategies)
6. [Precheck Detection](#precheck-detection)
7. [Implementation Phases](#implementation-phases)
8. [Configuration](#configuration)
9. [Testing Strategy](#testing-strategy)
10. [Performance Considerations](#performance-considerations)

---

## Overview

This document describes the design for **adaptive weighted routing** and **tiered backend selection** for the NNTP proxy. The goal is to maximize throughput by intelligently routing requests to the most appropriate backend servers based on:

1. **Real-time performance metrics** (pending commands, TTFB, error rates, hit rates)
2. **Server retention characteristics** (age/archival tiers)
3. **Client precheck behavior** (STAT/HEAD command detection)

### Key Insight: Precheck Detection

SABnzbd and similar clients can enable **"Check before download" (precheck)** mode, which sends lightweight `STAT` or `HEAD` commands before downloading articles. This provides:

- **Zero-cost article age detection** - No need to download full articles
- **Retention boundary learning** - 430 responses teach us which servers have which articles
- **Smart routing opportunities** - Route old articles to archive servers, recent to fast servers

**IMPORTANT:** The proxy only performs parallel precheck when it detects the client is sending `STAT` commands. If the client directly sends `ARTICLE`/`BODY`/`HEAD` commands without prechecking, the proxy **skips** the parallel precheck phase and routes directly. This prevents:

1. **Server lying issues** - Some servers incorrectly return 430 for STAT but successfully serve the article
2. **Performance overhead** - No wasted parallel queries for clients that don't precheck
3. **Backward compatibility** - Works seamlessly with clients that don't use precheck mode

---

## Goals

### Primary Goals

1. **Maximum Throughput** - Route to fastest available backend (not even distribution)
2. **Tiered Server Support** - Fast/premium servers for recent articles, archive servers for old content
3. **Retention Learning** - Automatically learn server retention boundaries from 430/423 responses
4. **Backward Compatibility** - Work with clients that do NOT send precheck commands

### Secondary Goals

1. **Zero Configuration** - Work out-of-the-box with sensible defaults
2. **Observable** - Expose routing decisions via metrics/logging
3. **Testable** - Property-based testing for routing correctness
4. **Safe** - Graceful degradation when learning data unavailable

---

## Background: SABnzbd's Approach

### How SABnzbd Routes by Article Age

From `sabnzbd/downloader.py` (lines 174-197):

```python
@synchronized(DOWNLOADER_LOCK)
def get_article(self):
    """Get article from pre-fetched and pre-fetch new ones if necessary.
    Articles that are too old for this server are immediately marked as tried"""
    if self.article_queue:
        return self.article_queue.pop(0)

    if self.next_article_search < time.time():
        # Pre-fetch new articles
        self.article_queue = sabnzbd.NzbQueue.get_articles(self, sabnzbd.Downloader.servers, _ARTICLE_PREFETCH)
        if self.article_queue:
            article = self.article_queue.pop(0)
            # Mark expired articles as tried on this server
            if self.retention and article.nzf.nzo.avg_stamp < time.time() - self.retention:
                sabnzbd.Downloader.decode(article)  # Skip this server
                while self.article_queue:
                    sabnzbd.Downloader.decode(self.article_queue.pop())
            else:
                return article
```

**Key takeaways:**

1. Article age comes from **NZB metadata** (`avg_stamp` field)
2. Server retention is **user-configured** (days to keep articles)
3. Pre-filters articles **before** sending ARTICLE/BODY commands
4. Prevents wasted 430 errors on servers that won't have old articles

### Precheck Mode

From `sabnzbd/newswrapper.py` (lines 165-179):

```python
def body(self):
    """Request the body of the article"""
    self.timeout = time.time() + self.server.timeout
    if self.article.nzf.nzo.precheck:
        if self.server.have_stat:
            command = utob("STAT <%s>\r\n" % self.article.article)
        else:
            command = utob("HEAD <%s>\r\n" % self.article.article)
    elif self.server.have_body:
        command = utob("BODY <%s>\r\n" % self.article.article)
    else:
        command = utob("ARTICLE <%s>\r\n" % self.article.article)
```

**When precheck is enabled:**
- Client sends `STAT <message-id>` (or `HEAD` if STAT unsupported)
- Server responds with **223** (exists) or **430** (doesn't exist)
- No article body downloaded - instant response
- Perfect for learning retention boundaries

---

## Architecture

### High-Level Flow

```
Client Request → Command Classifier → Routing Strategy Selector
                                              ↓
                                    ┌─────────┴─────────┐
                                    │                   │
                            Adaptive Weighted     Tiered Retention
                            (recent articles)     (old articles)
                                    │                   │
                                    └─────────┬─────────┘
                                              ↓
                                    Backend Selection
                                              ↓
                                    Connection Pool
                                              ↓
                                    NNTP Server
```

### Data Structures

```rust
/// Routing strategy configuration
pub enum RoutingStrategy {
    /// Traditional round-robin (current behavior)
    RoundRobin,
    
    /// Adaptive weighted routing based on real-time metrics
    AdaptiveWeighted {
        /// Weight for pending command count (0.0-1.0)
        pending_weight: f32,
        /// Weight for TTFB (0.0-1.0)
        ttfb_weight: f32,
        /// Weight for error rate (0.0-1.0)
        error_weight: f32,
        /// Weight for hit rate (0.0-1.0)
        hit_rate_weight: f32,
    },
    
    /// Tiered routing based on server retention
    TieredRetention {
        /// Primary servers (fast, recent articles)
        primary_tier: Vec<BackendId>,
        /// Archive servers (slow, old articles)
        archive_tier: Vec<BackendId>,
        /// Fallback behavior on 430/423
        fallback_enabled: bool,
    },
}

/// Backend learning data (retention boundaries, performance)
pub struct BackendLearning {
    /// User-configured retention in days (optional)
    retention_days: Option<u32>,
    
    /// Learned minimum article number that succeeded
    learned_min_article: Option<u64>,
    
    /// Learned maximum article number that succeeded
    learned_max_article: Option<u64>,
    
    /// Success rate per article number range
    success_rate_per_range: HashMap<ArticleRange, f64>,
    
    /// Total precheck requests sent (STAT/HEAD)
    precheck_requests: u64,
    
    /// Precheck 430 responses (article missing)
    precheck_misses: u64,
    
    /// Detected precheck support (server responds to STAT/HEAD)
    supports_precheck: Option<bool>,
}

/// Article number range for learning
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct ArticleRange {
    /// Start of range (inclusive)
    start: u64,
    /// End of range (inclusive)  
    end: u64,
}
```

---

## Routing Strategies

### 1. Adaptive Weighted Routing

**Formula:**
```
score = (pending/max) × 0.4 + (ttfb/100) × 0.3 + (error_rate/10) × 0.2 + (1 - hit_rate) × 0.1
```

**Select backend with LOWEST score.**

**Weights (configurable):**
- **Pending commands (40%)** - Avoid overloaded backends
- **TTFB (30%)** - Prefer fast-responding servers
- **Error rate (20%)** - Avoid unreliable backends
- **Hit rate (10%)** - Prefer servers that have articles

**⚠️ TTFB Limitation for STAT Commands:**

In practice, STAT command TTFB is nearly identical across backends and **independent of article age**. Observed values: **140-200ms regardless of backend or article characteristics**, making it **unreliable for measuring actual article retrieval speed**. Reasons:

1. **STAT is lightweight** - No article body transfer, just metadata lookup
2. **Network latency dominates** - RTT is the primary factor, not server performance
3. **No disk I/O** - Most servers cache article metadata in memory
4. **Age-independent** - Old and new articles show identical STAT response times

**Implications:**
- TTFB weight (30%) is most useful for **ARTICLE/BODY commands**, not STAT
- For STAT-only workloads, reduce TTFB weight to 0.1 or 0.0
- Consider tracking **separate TTFB metrics** for STAT vs ARTICLE commands
- **Alternative metric:** Track "article retrieval time" (time from ARTICLE to terminator) for real throughput measurement

**Implementation:**

```rust
pub fn select_backend_adaptive(&self) -> Option<BackendId> {
    self.backends
        .iter()
        .filter(|b| b.health_status == HealthStatus::Healthy)
        .min_by(|a, b| {
            let score_a = self.calculate_adaptive_score(a);
            let score_b = self.calculate_adaptive_score(b);
            score_a.partial_cmp(&score_b).unwrap_or(Ordering::Equal)
        })
        .map(|b| b.id)
}

fn calculate_adaptive_score(&self, backend: &BackendInfo) -> f64 {
    let pending_ratio = backend.pending_count.load(Ordering::Relaxed) as f64 
                        / backend.max_connections as f64;
    
    let ttfb_ms = backend.stats.average_ttfb_ms().unwrap_or(0.0);
    let error_rate = backend.stats.error_rate_percent();
    let hit_rate = backend.stats.hit_rate();
    
    // Lower score = better backend
    (pending_ratio * 0.4) 
        + (ttfb_ms / 100.0 * 0.3)
        + (error_rate / 10.0 * 0.2)
        + ((1.0 - hit_rate) * 0.1)
}
```

### 2. Tiered Retention Routing

**Strategy:**
1. **Estimate article age** from article number (lower = older)
2. **Route to appropriate tier:**
   - Recent articles → Primary tier (fast servers)
   - Old articles → Archive tier (bulk/cheap servers)
3. **Learn from 430 responses:**
   - Track which article ranges succeed on which backends
   - Update retention boundaries automatically

**Implementation:**

```rust
pub fn select_backend_tiered(&self, article_number: Option<u64>) -> Option<BackendId> {
    match article_number {
        Some(num) => {
            // Check if article is likely old (based on learned boundaries)
            if self.is_likely_old_article(num) {
                // Try archive tier first
                self.select_from_tier(&self.archive_tier)
                    .or_else(|| self.select_from_tier(&self.primary_tier))
            } else {
                // Try primary tier first
                self.select_from_tier(&self.primary_tier)
                    .or_else(|| self.select_from_tier(&self.archive_tier))
            }
        }
        None => {
            // No article number - use primary tier
            self.select_from_tier(&self.primary_tier)
        }
    }
}

fn is_likely_old_article(&self, article_number: u64) -> bool {
    // Check learned retention boundaries for each backend
    for backend in &self.backends {
        if let Some(learned_min) = backend.learning.learned_min_article {
            if article_number < learned_min {
                return true; // Older than oldest successful article
            }
        }
    }
    false
}
```

### 3. Fallback Chain

When a backend returns **430** or **423** (article not found):

```rust
pub fn handle_article_not_found(&self, backend_id: BackendId, article: &str) -> Option<BackendId> {
    // Mark this backend as tried for this article
    self.mark_tried(backend_id, article);
    
    // Try next backend in fallback chain
    self.backends
        .iter()
        .filter(|b| b.id != backend_id)
        .filter(|b| !self.was_tried(b.id, article))
        .filter(|b| b.health_status == HealthStatus::Healthy)
        .min_by_key(|b| self.calculate_adaptive_score(b))
        .map(|b| b.id)
}
```

---

## Precheck Detection

### Implementation Decision: STAT-Triggered Precheck Only

**IMPORTANT:** The proxy only performs parallel precheck probing when it receives **STAT commands** from the client. It does NOT proactively precheck for ARTICLE, BODY, or HEAD commands.

**Rationale:**

1. **Precheck is Client-Initiated:**
   - If the client sends STAT first, it's explicitly checking article existence before downloading
   - Proxy respects this workflow and can use parallel probing to learn backend availability
   - If the client sends ARTICLE directly, it's not using precheck - proxy shouldn't add it

2. **Avoids Performance Overhead:**
   - ARTICLE/BODY commands already transfer the article - adding precheck could slow things down
   - STAT is lightweight (small response), so parallel probing is cheap when client initiates it
   - Only beneficial when client is already using precheck mode (SABnzbd with adaptive_precheck)

3. **Maintains Backward Compatibility:**
   - Clients that don't use precheck (download directly with ARTICLE) see no behavior change
   - Clients that use precheck (SABnzbd with adaptive_precheck) get optimized routing
   - No risk of breaking existing workflows

### Automatic Detection

The proxy can detect when clients are using precheck mode by observing command patterns:

```rust
/// Precheck detector - identifies STAT/HEAD command patterns
pub struct PrecheckDetector {
    /// STAT commands sent in last 60 seconds
    stat_commands: VecDeque<Instant>,
    /// HEAD commands sent in last 60 seconds
    head_commands: VecDeque<Instant>,
    /// ARTICLE/BODY commands sent in last 60 seconds
    article_commands: VecDeque<Instant>,
}

impl PrecheckDetector {
    /// Detect if client is using precheck mode
    pub fn is_precheck_active(&self) -> bool {
        let stat_count = self.stat_commands.len();
        let head_count = self.head_commands.len();
        let article_count = self.article_commands.len();
        
        // Precheck mode: more STAT/HEAD than ARTICLE/BODY
        (stat_count + head_count) > article_count * 2
    }
}
```

### Parallel Precheck Probing

When a **STAT command** is received (and precheck mode is detected), send it to **one backend from each tier in parallel**:

```rust
pub async fn probe_backends_parallel(&self, message_id: &str) -> Vec<(BackendId, bool)> {
    let mut tasks = Vec::new();
    
    // Select one backend per tier
    let backends_to_probe = self.select_one_per_tier();
    
    for backend_id in backends_to_probe {
        let msg_id = message_id.to_string();
        let provider = self.get_provider(backend_id).clone();
        
        tasks.push(tokio::spawn(async move {
            let result = send_stat_command(&provider, &msg_id).await;
            (backend_id, result.is_ok())
        }));
    }
    
    // Wait for all probes
    let results = futures::future::join_all(tasks).await;
    results.into_iter().filter_map(|r| r.ok()).collect()
}
```

**Benefits:**
- Learn which backends have the article (for STAT commands only)
- Update retention boundaries based on STAT responses
- Route subsequent ARTICLE commands to backends that responded 223 to STAT
- Only activate when client demonstrates precheck workflow

---

## Implementation Phases

### Phase 1: Metrics Collection (CURRENT)

- ✅ Track pending commands per backend
- ✅ Track TTFB per backend (NOTE: STAT TTFB unreliable - see limitation above)
- ✅ Track error rates (4xx, 5xx, connection failures)
- ✅ Track hit rates (successful article retrievals)
- 🔲 **TODO:** Track separate TTFB for STAT vs ARTICLE commands
- 🔲 **TODO:** Track article retrieval time (ARTICLE command → terminator)
- 🔲 **TODO:** Implement exponential moving average (EMA) for article retrieval metrics
  - Replaces fixed time windows ("last hour") with decay-based weighting
  - Adapts to changing workload patterns (old→new→old article sequences)
  - Configurable decay factor (default: alpha = 0.1 for ~10-article memory)

### Phase 2: Adaptive Weighted Routing

1. Add `RoutingStrategy` enum to config
2. Implement `calculate_adaptive_score()`
3. Implement `select_backend_adaptive()`
4. Add unit tests with mock metrics
5. Add integration tests comparing round-robin vs adaptive

**Acceptance Criteria:**
- Under load, adaptive routing uses least-loaded backends 80%+ of the time
- TTFB decreases by 20%+ vs round-robin under high load
- All existing tests pass

### Phase 3: Precheck Detection

1. Implement `PrecheckDetector` state machine
2. Track STAT/HEAD vs ARTICLE/BODY ratio
3. Add metrics for precheck detection
4. Log when precheck mode detected

**Acceptance Criteria:**
- Correctly detects SABnzbd with precheck enabled within 10 commands
- Correctly detects SABnzbd with precheck disabled within 10 commands
- No false positives on other clients

### Phase 4: Retention Learning

1. Add `BackendLearning` struct
2. Track 430/423 responses per article number range
3. Implement `learned_min_article` and `learned_max_article`
4. Update boundaries on each 430 response

**Acceptance Criteria:**
- Learn retention boundaries within 100 STAT commands
- Correctly identify old vs recent articles 90%+ of the time
- Persist learned data across restarts (optional)

### Phase 5: Tiered Routing

1. Add `TieredRetention` strategy
2. Implement `select_backend_tiered()`
3. Implement fallback chain on 430/423
4. Add configuration for primary/archive tiers

**Acceptance Criteria:**
- Route recent articles to primary tier 95%+ of the time
- Route old articles to archive tier 90%+ of the time
- Fallback to other tier on 430 within 50ms

### Phase 6: Parallel Precheck Probing

1. Implement `probe_backends_parallel()`
2. Send STAT to one backend per tier simultaneously
3. Update routing based on probe results
4. Add metrics for probe success rates

**Acceptance Criteria:**
- Probes complete within 100ms for 2 backends
- Correctly routes subsequent ARTICLE to responding backend
- No duplicate article retrievals

---

## Configuration

### TOML Configuration

```toml
# Routing strategy (round-robin, adaptive-weighted, tiered-retention)
routing_strategy = "adaptive-weighted"

# Adaptive weighted routing configuration
[routing.adaptive_weighted]
pending_weight = 0.4
ttfb_weight = 0.3
error_weight = 0.2
hit_rate_weight = 0.1

# Tiered retention routing configuration
[routing.tiered_retention]
# Primary tier: fast servers for recent articles
primary_tier = ["server1", "server2"]
# Archive tier: bulk servers for old articles
archive_tier = ["server3", "server4"]
# Enable fallback to other tier on 430
fallback_enabled = true

# Backend server configurations
[[servers]]
host = "fast.example.com"
port = 563
name = "server1"
tier = "primary"  # NEW: tier classification
retention_days = 3000  # NEW: optional retention config

[[servers]]
host = "archive.example.com"
port = 563
name = "server3"
tier = "archive"
retention_days = 5000

# Precheck detection settings
[routing.precheck]
# Automatically detect precheck mode
auto_detect = true
# Parallel probe on STAT/HEAD commands
parallel_probe = true
# Number of backends to probe simultaneously
probe_count = 2
```

### CLI Flags

```bash
# Override routing strategy
nntp-proxy --routing-strategy adaptive-weighted

# Disable precheck detection
nntp-proxy --no-precheck-detection

# Force specific tier for testing
nntp-proxy --force-tier primary
```

---

## Testing Strategy

### Unit Tests

```rust
#[test]
fn test_adaptive_score_calculation() {
    let backend = create_test_backend();
    backend.pending_count.store(5, Ordering::Relaxed);
    backend.stats.record_ttfb(50.0);
    backend.stats.record_error(ErrorType::_4xx);
    
    let score = calculate_adaptive_score(&backend);
    assert!(score > 0.0);
    assert!(score < 1.0);
}

#[test]
fn test_tiered_routing_recent_article() {
    let router = create_test_router();
    router.configure_tiers(vec![0, 1], vec![2, 3]);
    
    // Recent article (high number)
    let backend = router.select_backend_tiered(Some(1_000_000));
    assert!(backend.unwrap().as_index() < 2); // Primary tier
}

#[test]
fn test_fallback_chain_on_430() {
    let router = create_test_router();
    
    // First backend returns 430
    let backend1 = router.select_backend_tiered(Some(500_000)).unwrap();
    let backend2 = router.handle_article_not_found(backend1, "test");
    
    assert!(backend2.is_some());
    assert_ne!(backend1, backend2.unwrap());
}
```

### Property-Based Tests

```rust
proptest! {
    #[test]
    fn prop_adaptive_always_selects_healthy_backend(
        pending_counts in vec![0u32..100, 4],
        ttfb_values in vec![0.0f64..500.0, 4]
    ) {
        let router = create_router_with_metrics(pending_counts, ttfb_values);
        let selected = router.select_backend_adaptive();
        
        // Selected backend must be healthy
        if let Some(backend_id) = selected {
            let backend = router.get_backend(backend_id);
            assert_eq!(backend.health_status, HealthStatus::Healthy);
        }
    }
}
```

### Integration Tests

```rust
#[tokio::test]
async fn test_adaptive_routing_under_load() {
    // Create proxy with adaptive routing
    let proxy = create_test_proxy(RoutingStrategy::AdaptiveWeighted);
    
    // Simulate 100 concurrent clients
    let tasks: Vec<_> = (0..100)
        .map(|_| tokio::spawn(send_article_request()))
        .collect();
    
    futures::future::join_all(tasks).await;
    
    // Verify load was distributed to least-loaded backends
    let snapshot = proxy.metrics.snapshot();
    let max_pending = snapshot.backend_stats.iter()
        .map(|s| s.pending_count)
        .max()
        .unwrap();
    
    let min_pending = snapshot.backend_stats.iter()
        .map(|s| s.pending_count)
        .max()
        .unwrap();
    
    // Max pending should be < 2x min pending (good distribution)
    assert!(max_pending < min_pending * 2);
}
```

---

## Performance Considerations

### Hot Path Optimization

Routing decision must complete in **< 1μs** to not impact throughput.

```rust
// ❌ BAD - Heap allocation on every route
pub fn select_backend(&self) -> Option<BackendId> {
    let mut scores = Vec::new();
    for backend in &self.backends {
        scores.push((backend.id, self.calculate_score(backend)));
    }
    scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    scores.first().map(|(id, _)| *id)
}

// ✅ GOOD - Zero allocations, iterator-based
pub fn select_backend(&self) -> Option<BackendId> {
    self.backends
        .iter()
        .filter(|b| b.health_status == HealthStatus::Healthy)
        .min_by(|a, b| {
            let score_a = self.calculate_score(a);
            let score_b = self.calculate_score(b);
            score_a.partial_cmp(&score_b).unwrap_or(Ordering::Equal)
        })
        .map(|b| b.id)
}
```

### Memory Usage

- **BackendLearning:** ~1KB per backend (HashMap with 100 ranges)
- **PrecheckDetector:** ~200 bytes per client (VecDeque of Instants)
- **Total overhead:** ~10KB for 10 backends + 10 clients

### Lock-Free Updates

All metrics use atomic operations:

```rust
// Update pending count (lock-free)
backend.pending_count.fetch_add(1, Ordering::Relaxed);

// Record TTFB (lock-free via lock-free aggregator)
backend.stats.record_ttfb(ttfb_ms);
```

---

## Open Questions

1. **Article number extraction:** How do we extract article numbers from message-IDs?
   - Some message-IDs embed article numbers: `<12345@server>`
   - Others use opaque IDs: `<abc123xyz@server>`
   - **Solution:** Track article numbers from GROUP/XOVER commands

2. **Retention boundary persistence:** Should we save learned data across restarts?
   - **Pros:** Faster warmup, retains knowledge
   - **Cons:** Stale data if server retention changes
   - **Solution:** Save with timestamp, expire after 7 days

3. **Precheck probe overhead:** Is parallel probing worth the extra connections?
   - **Pros:** Fast learning, accurate routing
   - **Cons:** 2x connection usage for STAT commands
   - **Solution:** Make configurable, default to OFF

4. **Fallback exhaustion:** What happens when all backends return 430?
   - **Solution:** Return 430 to client, log warning

5. **TTFB metrics separation:** Should we track STAT vs ARTICLE TTFB separately?
   - **Problem:** STAT TTFB is nearly identical across backends (network RTT dominated)
   - **STAT TTFB:** 140-200ms typically (observed), independent of article age, doesn't reflect retrieval speed
   - **ARTICLE TTFB:** Includes disk I/O, bandwidth, server load - more useful for routing
   - **Solution:** Track separate `stat_ttfb_ms` and `article_ttfb_ms` metrics
   - **Alternative:** Track "article retrieval time" (ARTICLE → terminator) for throughput measurement

6. **Article retrieval metrics time window:** What time interval should we track?
   - **Problem:** Simple time-based windows ("last hour") don't account for workload patterns
   - **Example:** If you retrieve old→new→old→new articles, hourly average is meaningless
   - **Challenge:** Article age distribution varies by client workload (sequential vs random access)
   - **Options:**
     - **Per-article-age buckets:** Track metrics separately for recent vs old articles
     - **Exponential moving average:** Weight recent retrievals more heavily (decay factor)
     - **Sliding window with article age grouping:** Last N articles per age range
     - **Workload-adaptive:** Different metrics for sequential (GROUP/NEXT) vs random (ARTICLE by message-ID)
   - **Recommendation:** Use **exponential moving average (EMA)** with configurable decay
     - Recent retrievals have more weight (e.g., last 100 articles = 90% of weight)
     - Old retrievals decay over time (e.g., 1000 articles ago = 1% weight)
     - Adapts to changing workload patterns automatically
     - Example: `EMA_new = alpha * current_value + (1 - alpha) * EMA_old` where `alpha = 0.1`

---

## Success Metrics

### Performance Targets

- **Routing decision time:** < 1μs (measured with Divan benchmarks)
- **TTFB improvement:** 20%+ vs round-robin under load
- **Hit rate improvement:** 15%+ with tiered routing
- **430 reduction:** 30%+ with retention learning

### Observability

New metrics exposed:

```rust
pub struct RoutingMetrics {
    /// Routing strategy used
    strategy: RoutingStrategy,
    /// Adaptive score per backend (real-time)
    adaptive_scores: Vec<f64>,
    /// Retention boundaries learned per backend
    retention_boundaries: Vec<(u64, u64)>,
    /// Precheck detection active
    precheck_active: bool,
    /// Parallel probe success rate
    probe_success_rate: f64,
    /// Fallback chain invocations
    fallback_count: u64,
}
```

Exposed via TUI:
- Current routing strategy
- Per-backend adaptive scores
- Learned retention ranges
- Precheck mode indicator

---

## References

- **SABnzbd source:** [github.com/sabnzbd/sabnzbd](https://github.com/sabnzbd/sabnzbd)
  - `sabnzbd/downloader.py` - Server.get_article() retention logic
  - `sabnzbd/newswrapper.py` - Precheck command generation
- **RFC 3977:** NNTP Protocol (STAT, HEAD, ARTICLE commands)
- **Load Balancing Algorithms:** [AWS ELB Docs](https://docs.aws.amazon.com/elasticloadbalancing/latest/userguide/how-elastic-load-balancing-works.html)
