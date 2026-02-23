# NNTP Proxy — Development Guidelines

> **Purpose:** Canonical reference for codebase patterns, architectural decisions, and mandatory conventions.
> **Audience:** AI assistants (Claude Code), human contributors, code reviewers.
> **Principle:** Reuse > Reimplementation. When a pattern exists, use it — don't duplicate it.

---

## Table of Contents

1. [Architectural Overview](#architectural-overview)
2. [Mandatory Patterns](#mandatory-patterns)
3. [Routing & Session Management](#routing--session-management)
4. [Performance Patterns](#performance-patterns)
5. [Error Handling](#error-handling)
6. [Testing & Benchmarking](#testing--benchmarking)
7. [Code Style & Idioms](#code-style--idioms)
8. [Anti-Patterns (Don't Do This)](#anti-patterns-dont-do-this)

---

## Architectural Overview

### Core Components

```
┌─────────────────────────────────────────────────────┐
│                 NNTP Proxy Server                   │
├─────────────────────────────────────────────────────┤
│ Runtime: tokio (async) + jemalloc (allocator)       │
│ Protocol: RFC 3977 NNTP + TLS (rustls)              │
└─────────────────────────────────────────────────────┘
           │
           ├──> Routing Layer (3 modes: Stateful, PerCommand, Hybrid)
           │    └─> BackendSelector: round-robin + health-aware
           │
           ├──> Connection Pool (deadpool)
           │    ├─> Pre-authenticated backend connections
           │    └─> Connection prewarming on startup
           │
           ├──> Caching Layer (2-tier)
           │    ├─> Memory cache: moka (LRU) or foyer-memory
           │    └─> Disk cache: foyer hybrid (psync I/O)
           │
           ├──> Buffer Management
           │    ├─> BufferPool: zero-alloc I/O scratch buffers
           │    ├─> PooledBuffer [acquire()]: single-read I/O scratch (724KB, pre-faulted)
           │    └─> PooledBuffer [acquire_capture()]: accumulator for caching (768KB, pre-faulted)
           │
           └──> Pipeline Engine (feature/tcp-command-pipelining)
                ├─> BackendQueue: batched ARTICLE/BODY requests
                └─> Batching: 4-16 commands per round-trip
```

### Routing Modes

1. **Stateful** (1:1 client↔backend mapping)
   - Full NNTP protocol support
   - Dedicated backend connection per client
   - Used for: GROUP, XOVER, article-by-number commands

2. **PerCommand** (pure stateless routing)
   - Each command routes to any available backend
   - Only supports message-ID based operations
   - Most resource-efficient (shared pool)

3. **Hybrid** (default, intelligent auto-switching)
   - Starts in PerCommand mode
   - Auto-switches to Stateful when client issues GROUP/XOVER/etc.
   - Best of both: efficiency + compatibility

---

## Mandatory Patterns

### 1. Terminator Detection

**`TailBuffer` is the ONE AND ONLY place terminator detection can exist in this codebase. There must never be any other implementation anywhere.**

ANNTP multiline response ends with `\r\n.\r\n` (5 bytes). Detecting this correctly across async chunk reads is subtle and easy to get wrong. Getting it wrong causes **connection pool collapse** (see below).

#### Forbidden Patterns — Never Write These

```rust
// ❌ ends_with check on accumulated buffer
if data.ends_with(b"\r\n.\r\n") { ... }
if data.ends_with(b".\r\n") { ... }

// ❌ Private helper that wraps ends_with
fn has_terminator(data: &[u8]) -> bool {
    data.ends_with(b"\r\n.\r\n")
}

// ❌ Manual byte loop scanning for terminator bytes
fn find_terminator(buf: &[u8], start: usize) -> Option<usize> {
    for i in start..buf.len().saturating_sub(4) {
        if &buf[i..i+5] == b"\r\n.\r\n" { return Some(i); }
    }
    None
}

// ❌ Constant replicated outside tail_buffer.rs
const MULTILINE_TERMINATOR: &[u8] = b"\r\n.\r\n";
const TERMINATOR: &[u8] = b"\r\n.\r\n";

// ❌ has_multiline_terminator() wrapper function
pub fn has_multiline_terminator(data: &[u8]) -> bool { ... }

// ❌ Accumulate-then-check loop (the worst pattern)
let mut accumulated = Vec::new();
loop {
    let n = stream.read(&mut buf).await?;
    accumulated.extend_from_slice(&buf[..n]);
    if accumulated.ends_with(b"\r\n.\r\n") { break; } // WRONG
}
```

#### Correct: Streaming (most common case)

```rust
use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

// Create ONE TailBuffer per response — use it across ALL chunks
let mut tail = TailBuffer::default();

loop {
    let n = stream.read(buffer.as_mut_slice()).await?;
    if n == 0 { break; }
    let chunk = &buffer[..n];

    match tail.detect_terminator(chunk) {
        TerminatorStatus::FoundAt(pos) => {
            // Write only up to (and including) the terminator
            // CRITICAL: pos is byte offset of '\r' in "\r\n.\r\n"
            // Do NOT write chunk[pos..] — that's the start of the next response
            client.write_all(&chunk[..pos]).await?;
            break;
        }
        TerminatorStatus::NotFound => {
            client.write_all(chunk).await?;
            tail.update(chunk); // Keep tail state for boundary detection
        }
    }
}
```

#### Correct: Complete-buffer validation (already fully accumulated)

When you have a fully-accumulated `Vec<u8>` (e.g., validating a cache entry that was already read) and just want to confirm it ends with the terminator:

```rust
use crate::session::streaming::tail_buffer::TailBuffer;

// TailBuffer with no prior state, checking the complete buffer at once
TailBuffer::default().detect_terminator(&buffer).is_found()
```

This is correct because a fresh `TailBuffer` with no prior tail state applied to the full buffer is equivalent to a full-buffer terminator check — but still handles the case where `\r\n.\r\n` appears mid-buffer (e.g., article followed by pipelined data).

#### Why `ends_with()` Causes Connection Pool Collapse

This is not a theoretical concern — it caused a production bug where 20 connections were removed from the pool in rapid succession, exhausting the pool entirely:

1. Backend sends article body followed immediately by the start of the next pipelined response in the same TCP segment
2. `stream.read()` returns a single chunk containing `[...article data...\r\n.\r\n200 Article follows\r\n...]`
3. `accumulate.ends_with(b"\r\n.\r\n")` returns **false** because the buffer ends with `...200 Article follows\r\n...`, not with the terminator
4. Loop continues reading — now it consumes the `220 0 <next-msg-id>\r\n` response for the **next** pipelined command
5. The next pipelined command's response handler reads something that makes no sense — returns `Invalid`
6. Connection marked broken → `remove_with_cooldown()` → pool size temporarily reduced
7. Under load with 40 connections and many pipelined commands: cascade to zero

`TailBuffer::detect_terminator()` returns `FoundAt(pos)` pointing to the `\r` of `\r\n.\r\n`, so you write only up to that position and stop — the pipelined response bytes are never consumed from the wrong context.

**Location:** `src/session/streaming/tail_buffer.rs` (30+ tests, production-proven)

**Pattern usage:**
- All streaming responses: `stream_multiline_response_impl`
- Cache precheck queries: `precheck.rs`
- Client-side article fetching: `client/mod.rs`
- Cache entry completeness validation: `cache/entry_helpers.rs`, `cache/article.rs`
- Article parsing: `protocol/article/mod.rs`

---

### 2. I/O Buffer Management

`BufferPool` has **two distinct modes**. Using the wrong one causes silent reallocation and/or corrupted data.

#### Mode 1: Scratch buffers — `acquire()`

For single read operations. The buffer is pre-allocated as a fixed-size `Vec<u8>` with `len == capacity` (724KB, page-faulted).

**✅ ALWAYS use for socket reads:**
```rust
let mut buffer = buffer_pool.acquire().await;
let n = buffer.read_from(stream).await?;
let chunk = &buffer[..n]; // Only initialized portion
// buffer automatically returned to pool on drop
```

**❌ NEVER do this with a scratch buffer:**
```rust
let mut buf = buffer_pool.acquire().await;
// BAD: Vec already has len=724KB — extend_from_slice() grows BEYOND capacity,
// causing realloc beyond the pre-faulted pages, and breaks pool debug_assert on return
buf.extend_from_slice(data);

// BAD: Allocates per-request (defeats the pool entirely)
let mut scratch = vec![0u8; 8192];
stream.read(&mut scratch).await?;
```

#### Mode 2: Capture buffers — `acquire_capture()`

For accumulating an entire streaming response (caching path only). The buffer is a pre-faulted **empty** `Vec<u8>` with `len == 0` and `capacity == 768KB`.

**✅ ONLY use for the caching capture path:**
```rust
// Only under CacheAction::CaptureArticle in command_execution.rs
let mut captured = self.buffer_pool.acquire_capture().await;
streaming::stream_and_capture_multiline_response(
    backend, client, first_chunk, ..., &mut captured
).await?;
self.maybe_cache_upsert(msg_id, &captured, backend_id);
```

**❌ NEVER use for I/O scratch in hot paths** — capture buffers are exclusively for caching.

#### Summary Table

| Need | Method | Buffer state | Use `extend_from_slice`? |
|---|---|---|---|
| Socket read | `acquire()` | len=724KB (pre-set) | ❌ Never |
| Cache accumulate | `acquire_capture()` | len=0, cap=768KB | ✅ Correct |

**Location:** `src/pool/buffer.rs`

---

### 3. Error Classification

**✅ ALWAYS use centralized error kind constants** from `connection_error.rs`.

**❌ NEVER inline:**
```rust
// BAD: Will diverge across the codebase
use std::io::ErrorKind;
match e.kind() {
    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => true,
    _ => false,
}
```

**✅ DO write:**
```rust
use crate::connection_error::{is_disconnect_kind, is_connection_error_kind};

// For raw io::Error
if is_disconnect_kind(e.kind()) { /* client gone */ }

// For wrapped anyhow::Error (common in handlers)
use crate::is_client_disconnect_error; // Re-exported from proxy/mod.rs
if is_client_disconnect_error(&e) { /* client disconnected */ }
```

**Canonical definitions** (`src/connection_error.rs`):
```rust
/// Errors indicating client disconnected (don't log as backend failures)
pub const DISCONNECT_KINDS: &[ErrorKind] = &[
    ErrorKind::BrokenPipe,
    ErrorKind::ConnectionReset,
];

/// Errors indicating connection is broken (don't return to pool)
pub const CONNECTION_ERROR_KINDS: &[ErrorKind] = &[
    ErrorKind::BrokenPipe,
    ErrorKind::ConnectionReset,
    ErrorKind::ConnectionAborted,
    ErrorKind::UnexpectedEof,
];
```

**Why:**
- Three separate inline checks diverged: one missing `ConnectionReset`, different sets used
- Centralized constants = compile-time guarantee they stay in sync
- Semantic clarity: `is_disconnect_kind()` vs raw match

**Location:**
- Constants: `src/connection_error.rs`
- Re-export for handlers: `src/proxy/mod.rs`

---

### 4. Status Code Checks

**✅ ALWAYS use pre-parsed `status_code` fields or `StatusCode::parse()`**.

**❌ NEVER write:**
```rust
// BAD: Fragile byte-prefix checks
if response.data.starts_with(b"430") { /* no such article */ }
```

**✅ DO write:**
```rust
// Use typed status codes
if response.status_code == 430 { /* no such article */ }

// Or parse from raw response
use crate::protocol::responses::StatusCode;
let status = StatusCode::parse(response_bytes)?;
if status.code() == 430 { /* ... */ }
```

**Why:**
- `starts_with(b"430")` false-positives on `4300`, `430 ignored text`, etc.
- Pre-parsed fields (added in pipeline refactoring) eliminate fragility
- Typos like `b"430"` vs `b"430 "` become impossible

**Pattern usage:**
- Pipeline responses carry `status_code: u16` field
- Cache key classification
- Metrics recording (which code to count)

---

### 5. Command Classification

**✅ ALWAYS use classifier helpers** from `src/command/classifier.rs`.

**❌ NEVER inline:**
```rust
// BAD: Duplicated memchr + matches_any pattern
use memchr::memchr;
let end = memchr(b' ', cmd).unwrap_or(cmd.len());
if end >= 4 && matches_any(&cmd[..end], ARTICLE_CASES) { /* ... */ }
```

**✅ DO write:**
```rust
use crate::command::classifier::{
    is_large_transfer_command,
    is_stat_command,
    is_head_command,
};

if is_large_transfer_command(cmd) {
    // Route to pipeline queue for batching
}
```

**Available helpers:**
- `is_large_transfer_command()` — ARTICLE/BODY (multi-line responses)
- `is_stat_command()` — STAT (single-line, no body)
- `is_head_command()` — HEAD (headers only)

**Why:**
- The `memchr + matches_any` pattern was duplicated 4 times
- When command detection logic changes, update once (not 4 places)
- All helpers are `#[inline(always)]` — zero overhead

**Location:** `src/command/classifier.rs`

---

### 6. Metrics Recording

**✅ ALWAYS use `record_response_metrics()`** for the determine_metrics_action pattern.

**❌ NEVER inline:**
```rust
// BAD: Duplicates the match block
match self.determine_metrics_action(&command, &response) {
    Some(MetricsAction::RecordHit) => { /* ... */ }
    Some(MetricsAction::RecordMiss) => { /* ... */ }
    None => {}
}
```

**✅ DO write:**
```rust
self.record_response_metrics(&command, &response);
```

**Why:**
- The metrics recording pattern appears identically in multiple handlers
- When metrics logic changes (e.g., new status codes tracked), update once
- Centralized in `command_execution.rs:378-406`

---

### 7. Protocol Constants

**✅ ALWAYS use constants from `src/protocol/responses.rs`** for protocol literals.

**Available:**
```rust
pub const CRLF: &[u8] = b"\r\n";
pub const TERMINATOR_TAIL_SIZE: usize = 4; // bytes TailBuffer keeps across chunks
```

**⚠️ NOT available — these were deleted because they caused bugs:**
- `MULTILINE_TERMINATOR` — deleted. Do not reintroduce. Use `TailBuffer` (see Mandatory Pattern #1).
- `has_multiline_terminator()` — deleted. Do not reintroduce. Use `TailBuffer`.

**Note:** The 3-byte `.\r\n` check (without leading `\r\n`) is for **line-based readers** only — different semantics from the 5-byte terminator used for **chunk-based detection**.

**Why:**
- Having a named constant + helper function for the terminator bytes creates an irresistible footgun: callers use `ends_with(MULTILINE_TERMINATOR)` which misses cross-boundary terminators and consumes pipelined data
- `TailBuffer` is the only correct interface; exposing the raw bytes encourages bypassing it

---

## Routing & Session Management

### Stateful vs PerCommand Patterns

**Stateful mode** (1:1 client→backend mapping):
```rust
// Takes generic W: AsyncWrite + Unpin
pub async fn run_stateful_proxy_loop<W>(
    client_writer: W,
    backend: BackendConnection,
    // ...
) -> Result<ProxyLoopResult>
```
- **Can** wrap `client_writer` in `BufWriter` for batching
- **MUST** call `.flush().await?` after auth responses and backend writes
- Used for: GROUP, XOVER, article-by-number commands

**PerCommand mode** (stateless, shared pool):
```rust
// Uses concrete WriteHalf<'_> (no generic)
pub async fn handle_per_command_routing(
    client_writer: &mut WriteHalf<'_>,
    // ...
)
```
- Changing to generic cascades through many signatures
- Each command acquires connection from pool, releases after response

**Hybrid mode detection:**
- Starts in PerCommand
- Commands like GROUP/XOVER trigger `SwitchToStateful` result
- Main loop acquires dedicated backend and calls `run_stateful_proxy_loop`

---

### Connection Pool Patterns

**Backend reservation:**
```rust
let mut backend = self.pool.get_stateful_reservation().await?;
// Connection locked to this session, not returned to pool
```

**One-shot command (PerCommand):**
```rust
let conn = self.pool.get().await?;
let response = execute_command(&conn, cmd).await?;
// Connection automatically returned to pool
```

**Prewarming:**
- Pool pre-authenticates connections on startup (faster first request)
- Configured via `prewarm_connections` in config

---

## Performance Patterns

### Hot Path Optimization

**1. Double-buffering with `tokio::join!`**
```rust
// Overlap read-from-backend + write-to-client
tokio::select! {
    read_result = backend.read(&mut buffer) => { /* ... */ }
    write_result = client.write_all(&prev_chunk) => { /* ... */ }
}
```
- Reduces syscall latency (sendto/recvfrom overlap)
- Used in `stream_multiline_response_impl`

**2. SIMD-accelerated scanning**
- `TailBuffer` uses `memchr` crate (SIMD on x86-64)
- Faster than byte-by-byte scan for terminator detection

**3. Zero-copy streaming**
- Stream backend→client without full buffering in proxy memory
- Only accumulate when caching (separate code path)

**4. Inline hints**
- Aggressive `#[inline(always)]` on hot path functions
- **Note:** No benchmark data comparing with/without hints (potential cargo-cult optimization)
- Used extensively (378 attributes across 54 files)

---

### Memory Allocation

**Avoid allocations in hot paths:**
- ✅ Use `BufferPool` for I/O scratch
- ✅ Reuse `Vec` with `clear()` instead of allocating new
- ✅ Pre-allocate with capacity hints when size known

**When to allocate:**
- Cache entries (long-lived, one-time cost)
- Error paths (rare, acceptable overhead)
- Initial setup (connection handshake, auth)

---

### Caching Strategy

**Two-tier cache:**
1. **Memory cache** (hot data):
   - moka (LRU) or foyer-memory
   - Configured via `memory_cache_capacity` (bytes or item count)

2. **Disk cache** (warm data):
   - foyer hybrid cache with **psync I/O engine**
   - ❌ **DO NOT use io_uring engine** — has busy-poll bug (53% idle CPU in tight try_recv loop)
   - Configured via `disk_cache_path` + `disk_cache_capacity`

**Cache key:**
- Message-ID (article content)
- STAT/HEAD/ARTICLE/BODY variants cached separately (same message-ID, different response)

**Cache invalidation:**
- TTL-based (configurable per cache)
- No active invalidation (Usenet articles immutable once posted)

---

### Thread Configuration

**ThreadCount pattern:**
```rust
use crate::config::ThreadCount;

// Auto-detect CPU count (0 → runtime detection)
let threads = ThreadCount::from_value(0); // Returns Some(N)

// Explicit count
let threads = ThreadCount::new(4); // Some(4)

// Note: ThreadCount::new(0) → None (const limitation, can't detect at compile time)
```

**Why the split:**
- `from_value(0)` calls `num_cpus::get()` at runtime
- `new(0)` is const-compatible but can't do runtime detection

---

## Metrics Persistence Pattern

### MetricsStore Design

**Why:** TUI dashboard metrics reset on proxy restart. `MetricsStore` persists cumulative counters to JSON every 30 seconds and on shutdown, restoring them on startup.

**Architecture:**
```
MetricsCollector (public API — unchanged)
└── inner: Arc<MetricsInner>
    ├── store: MetricsStore (persistable cumulative counters)
    │   ├── total_connections: AtomicU64
    │   ├── backend_stores: Vec<BackendStore>  ← per-backend cumulative metrics
    │   ├── pipeline_*: AtomicU64 fields
    │   └── user_metrics: DashMap<String, UserMetrics>
    ├── active_connections: AtomicUsize  ← live gauge (NOT persisted)
    ├── stateful_sessions: AtomicUsize   ← live gauge (NOT persisted)
    └── start_time: Instant              ← NOT persisted

BackendMetrics (wraps persistable store + live gauges)
├── store: BackendStore (persistable atomic counters)
├── active_connections: AtomicUsize  ← live gauge
└── health_status: BackendHealthStatus  ← live gauge
```

### File Format (Versioned JSON)

```json
{
  "version": 1,
  "saved_at": "2026-02-18T18:45:00Z",
  "global": { "total_connections": 42 },
  "backends": [
    { "name": "server1", "total_commands": 1000, "bytes_sent": 50000, ... },
    { "name": "server2", "total_commands": 900, "bytes_sent": 45000, ... }
  ],
  "users": [
    { "username": "alice", "total_connections": 20, "bytes_sent": 25000, ... }
  ],
  "pipeline": { "batches": 100, "commands": 1600, ... }
}
```

**Version migration:**
- Each version gets its own deserializable struct (e.g., `PersistedBackend`)
- `MetricsStore::load()` checks version, migrates old formats to current
- Always write latest version; old versions migrated on load
- Future v2 can add fields (e.g., retention tracking) — v1→v2 migration defaults new fields to zero

### Integration Points

**1. Binary startup (both nntp-proxy and nntp-proxy-tui):**
```rust
let stats_path = resolve_stats_file_path(config_path, config.proxy.stats_file);
let metrics_store = load_metrics_from_disk(&stats_path, &server_names);

let proxy = NntpProxy::builder(config)
    .with_metrics_store(metrics_store)  // Pass restored store
    .build()
    .await?;
```

**2. Periodic saver (spawned after prewarm):**
```rust
tokio::spawn(async move {
    let mut interval = interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        proxy.metrics().save_to_disk(&stats_path, &server_names).ok();
    }
});
```

**3. Shutdown save (in spawn_shutdown_handler):**
```rust
proxy.metrics().save_to_disk(&stats_path, &server_names).ok();
proxy.graceful_shutdown().await;
```

### Configuration

**config.toml:**
```toml
[proxy]
# Optional: custom stats file location
# Defaults to "stats.json" alongside config file if not specified
stats_file = "/var/lib/nntp-proxy/stats.json"
```

### Testing Strategy

**Tests in `src/metrics/store.rs`:**
1. `test_metrics_store_save_load_roundtrip` — verify save/load cycle preserves data
2. `test_metrics_store_backend_name_mapping` — backends matched by name across save/load
3. `test_metrics_store_missing_file_returns_none` — graceful missing file handling
4. `test_metrics_store_corrupt_file_returns_none` — graceful corruption handling
5. `test_metrics_store_backend_mismatch` — servers added/removed between save/load

**Serde tests in `src/metrics/mod.rs`:**
- `test_backend_stats_serde_roundtrip` — ephemeral fields default correctly on deserialize

### Extension Points (Future Use)

**For retention tracking (separate branch):**
- Add `oldest_served_article_age_secs`, `newest_missing_article_age_secs` to `PersistedBackend` in v2
- Migration function v1→v2 defaults these to 0
- TUI can query these fields for inventory analytics

---

## Error Handling

### Error Types

**1. ConnectionError** (`src/connection_error.rs`):
- Wraps I/O errors, TLS errors, protocol violations
- Implements `is_client_disconnect()` using centralized kind checks

**2. anyhow::Error** (handlers):
- Used for general error propagation in session handlers
- Check with `is_client_disconnect_error()` (re-exported from `proxy/mod.rs`)

**3. Protocol errors** (RFC 3977 violations):
- Return NNTP error responses to client (e.g., `502 Command not permitted`)
- Don't terminate connection unless fatal

---

### Error Classification Strategy

**Client disconnects** (don't log as backend failures):
- `BrokenPipe`, `ConnectionReset`
- Expected during normal operation (client closes connection)

**Connection errors** (don't return to pool):
- `BrokenPipe`, `ConnectionReset`, `ConnectionAborted`, `UnexpectedEof`
- Mark backend connection as unusable

**Backend failures** (log, attempt failover):
- TLS handshake failures
- Authentication failures
- Unexpected protocol responses

---

### Panic Safety

**Unsafe code locations:**
- `src/pool/buffer.rs` — buffer pool uses `MaybeUninit` for performance
- Carefully audited, no other unsafe blocks

**UTF-8 handling:**
- ❌ **NEVER slice strings at byte offsets** without checking boundaries
- ✅ **Use `chars().take(N).collect()`** for character-based truncation (see `format_username` in TUI)

---

## Testing & Benchmarking

### Test Organization

**By feature:**
```
tests/
├── auth/                   # AUTHINFO tests
├── cache/                  # Cache hit/miss scenarios
├── proxy/
│   ├── routing/            # Routing mode tests
│   └── pipeline/           # TCP pipelining tests
└── test_helpers.rs         # Shared utilities
```

**Test count:** ~2057 tests (run with `cargo nextest run`)

**Shared helpers:**
- Use `tests/test_helpers.rs` for common setup
- ❌ **NEVER duplicate helper functions** per test file

---

### Test Patterns

**1. Use `Server::builder()` in tests**

**❌ BAD:**
```rust
// 17-field manual construction, hardcodes wrong defaults
let server = Server {
    port: 8119,
    host: "127.0.0.1".to_string(),
    pipeline_batch_size: 16, // Wrong: production default is 4
    // ... 14 more fields ...
};
```

**✅ GOOD:**
```rust
let server = Server::builder()
    .port(8119)
    .host("127.0.0.1")
    .build();
// Automatically gets correct production defaults
```

**Why:**
- Tests track production defaults automatically
- When new fields added, tests get correct defaults (not struct literal compile errors)

**2. Use `..Default::default()` for large structs**

**❌ BAD:**
```rust
// Hardcodes all zero/empty values
let snapshot = MetricsSnapshot {
    cache_hits: 0,
    cache_misses: 0,
    // ... 20 more zero fields ...
};
```

**✅ GOOD:**
```rust
let snapshot = MetricsSnapshot {
    cache_hits: 10,
    ..Default::default()
};
```

---

### Benchmarking Rules

**❌ NEVER define local reimplementations in benchmarks:**
```rust
// BAD: Benchmark measures wrong code, can diverge
fn find_terminator_new(data: &[u8]) -> Option<usize> {
    // Local copy of production logic
}
```

**✅ ALWAYS import production functions:**
```rust
use nntp_proxy::session::streaming::tail_buffer::find_terminator_end;

#[divan::bench]
fn bench_terminator_detection() {
    divan::black_box(find_terminator_end(data));
}
```

**Why:**
- Benchmark must measure **actual production code**
- Local copies diverge (benchmark passes, production has bug)

**Baseline comparators:**
- If benchmarking old vs new implementation, name explicitly: `find_terminator_old` vs `find_terminator_new`
- Keep old implementation as historical reference, not as production code

---

### Performance Testing

**Before performance-sensitive changes:**
1. Run `cargo bench` and save baseline
2. Make changes
3. Run `cargo bench` again and compare

**Relevant benchmarks:**
- `cargo bench --bench command_parsing` — command classification
- `cargo bench --bench response_parsing` — terminator detection
- `cargo bench --bench router_selection` — routing logic

**Acceptance criteria:** Identical or faster (no regressions).

---

### Profiling Analysis

**Custom profiling scripts** in `scripts/` directory provide structured analysis of `perf.data`:

#### parse_perfdata

Analyzes `perf script` output to show hotspots, thread distribution, and timeline analysis.

**Usage:**
```bash
# Generate perf.data first
perf record -g ./target/release/nntp-proxy-tui

# Analyze with parse_perfdata
perf script 2>/dev/null | ./scripts/parse_perfdata
```

**Output sections:**
- **Thread Breakdown**: Shows CPU distribution across threads (tokio workers, foyer-disk-io, etc.)
- **Top Functions (self time)**: Functions consuming most CPU (excludes child functions)
- **Top Functions Per Thread**: Per-thread hotspot analysis
- **Caller → Callee Edges**: Call graph showing where time flows
- **Timeline**: Sample distribution over time (cold vs hot phase)
- **Category Summary**: Time grouped by category (Network I/O, TLS/Crypto, Cache, etc.)

**Key insights:**
- **Self time** shows actual work in function (not including callees)
- **Timeline** shows if performance degrades over time (hot phase worse = overhead)
- **Category summary** identifies system-level bottlenecks

**Example output interpretation:**
```
Top Functions (self time):
  8.07%  __memmove_avx_unaligned_erms  ← Memory copy hotspot
  4.80%  aes_gcm_dec_update            ← TLS decryption
  2.24%  Checksummer::checksum64       ← Cache overhead
```

Timeline showing hot phase worse:
```
First half:  7.44% memmove  (cold/startup)
Second half: 8.88% memmove  (hot/cached) ← Cache adds overhead!
```

#### parse_flamegraph

Generates SVG flamegraph from `perf script` output.

**Usage:**
```bash
# Generate flamegraph
perf script 2>/dev/null | ./scripts/parse_flamegraph > flamegraph.svg

# View in browser
firefox flamegraph.svg
```

**Flamegraph interpretation:**
- **Width**: Time spent in function (wider = more CPU)
- **Height**: Stack depth (nesting level)
- **Click function**: Zoom to subtree
- **Search (Ctrl+F)**: Highlight specific functions

**Tips:**
- Look for wide blocks at any height (not just top)
- Repetitive patterns indicate functions called many times
- Compare flamegraphs before/after changes

#### Profiling Workflow

**1. Baseline profile:**
```bash
# Start proxy
./target/release/nntp-proxy-tui &

# Run workload (e.g., sabnzbd download)
# ...

# Capture profile
perf record -g -p $(pgrep nntp-proxy-tui)
# Let run for 30-60 seconds, then Ctrl+C

# Analyze
perf script 2>/dev/null | ./scripts/parse_perfdata > baseline.txt
```

**2. After optimization:**
```bash
# Repeat profiling with same workload
perf script 2>/dev/null | ./scripts/parse_perfdata > optimized.txt

# Compare
diff baseline.txt optimized.txt
```

**3. Identify next target:**
- Look for functions with high self time (>2%)
- Check timeline for degradation (hot phase worse than cold)
- Category summary shows system-level bottlenecks
- Caller→Callee edges show where time flows

**Common optimization targets:**
- High memmove % → Look for unnecessary allocations or copies
- Cache category high → Cache overhead may exceed benefit
- Locks/Futex high → Contention, consider lock-free structures
- TLS/Crypto high → Expected, but verify hardware acceleration enabled

---

### Special Test Cases

**Cache tests with foyer HybridCache:**
```rust
#[ignore] // Hangs in test context
#[tokio::test]
async fn test_hybrid_cache() { /* ... */ }
```
- foyer disk cache doesn't work in test harness (tempfile cleanup issues)
- Mark as `#[ignore]`, test manually in integration environment

**Auth tests with BufWriter:**
- When wrapping writers in `BufWriter`, **MUST** call `.flush().await?` after:
  - Auth responses (AUTHINFO responses)
  - Backend→client writes
- Without flush, responses buffered and tests timeout waiting

---

## Code Style & Idioms

### Rust Idioms

**1. Enums over booleans for state**

**❌ BAD:**
```rust
let batch_handled = false;
// ... later ...
if !batch_handled {
    // Process single command
}
```

**✅ GOOD:**
```rust
enum SingleCommandResult {
    Continue { auth_succeeded: bool },
    Quit(u64),
    SwitchToStateful,
}

match result {
    SingleCommandResult::Continue { auth_succeeded } => { /* ... */ }
    SingleCommandResult::Quit(bytes) => return Ok(bytes),
    SingleCommandResult::SwitchToStateful => { /* ... */ }
}
```

**Why:**
- Compiler enforces exhaustive matching
- Self-documenting (what does `false` mean? vs explicit enum variant)

---

**2. Prefer `while let` over indexed loops**

**❌ BAD:**
```rust
for i in 0..batch_len {
    let req = batch_iter.next().unwrap();
    // Manual index tracking + iterator — redundant
}
```

**✅ GOOD:**
```rust
let mut batch_iter = batch.into_iter().enumerate();
while let Some((i, req)) = batch_iter.next() {
    // Iterator drives the loop naturally
}
```

---

**3. Extract method for repeated patterns**

**When you see the same 5+ line block twice, extract a method:**
```rust
// Pattern appears at lines 137-143 and 331-337
fn record_article_missing(&self, msg_id: &str, backend_id: BackendId, router: &Router) {
    if let Some(avail) = router.load_availability() {
        avail.record_missing_article(msg_id, backend_id);
        router.sync_availability(&avail);
    }
}
```

---

### Function Signatures

**1. Avoid `clippy::too_many_arguments` with context structs**

**❌ BAD:**
```rust
#[allow(clippy::too_many_arguments)]
fn stream_impl<R, W>(
    reader: &mut R,
    writer: &mut W,
    buffer_pool: &BufferPool,
    cache: &Cache,
    metrics: &Metrics,
    msg_id: &str,
    backend_id: BackendId,
    capture: bool,
) -> Result<()>
```

**✅ GOOD:**
```rust
struct StreamContext<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
    buffer_pool: &'a BufferPool,
    cache: &'a Cache,
    metrics: &'a Metrics,
    msg_id: &'a str,
    backend_id: BackendId,
}

fn stream_impl<R, W>(ctx: StreamContext<'_, R, W>, capture: bool) -> Result<()>
```

**Why:**
- Groups related parameters
- Easier to add new fields (append to struct, not function signature)
- Zero-cost (inlined, passed by reference)

---

**2. Remove unused parameters (compiler-checked dead code)**

**❌ BAD:**
```rust
fn execute_command(
    _client_reader: &ClientReader, // Destructured as unused
    client_writer: &mut ClientWriter,
) {
    // Never uses client_reader
}
```

**✅ GOOD:**
```rust
fn execute_command(
    client_writer: &mut ClientWriter,
) {
    // Removed unused parameter
}
```

**Why:**
- Compile error if code later tries to use the removed parameter → safe refactoring
- Documents actual dependencies

---

### Documentation

**Doc comments on public items:**
- Explain **why**, not **what** (code shows what)
- Link to RFCs when implementing protocol features
- Example usage for non-obvious APIs

**Inline comments:**
- Only where logic isn't self-evident
- Explain protocol quirks (e.g., "NNTP requires CRLF, LF alone is invalid")
- Avoid stating the obvious (e.g., `// Increment counter` before `count += 1`)

---

## Anti-Patterns (Don't Do This)

### ❌ 1. Any Terminator Detection Outside TailBuffer

**There is exactly one place terminator detection can exist: `TailBuffer::detect_terminator()` in `src/session/streaming/tail_buffer.rs`.**

Any occurrence of the following anywhere else in the codebase is a bug:
- `ends_with(b"\r\n.\r\n")` or `ends_with(b".\r\n")`
- `MULTILINE_TERMINATOR` or `TERMINATOR` constants containing terminator bytes
- `has_multiline_terminator()` or `has_terminator()` helper functions
- Any loop that manually scans bytes looking for `'.'` or `'\r'` to find the terminator
- An accumulate-then-check pattern (accumulate into Vec, check ends_with in loop condition)

This caused real connection pool collapse in production: `ends_with()` failed to stop at the terminator boundary, consumed pipelined response bytes into the wrong buffer, caused the next command to receive garbage, marked connections invalid, and cascaded to pool exhaustion. See Mandatory Pattern #1 for the full explanation.

**Historical locations that were deleted — do not recreate:**
- `src/protocol/responses.rs: MULTILINE_TERMINATOR`, `has_multiline_terminator()` — deleted
- `src/client/mod.rs: TERMINATOR` const — deleted
- `src/protocol/article/mod.rs: find_terminator()` — deleted
- `src/session/precheck.rs`: accumulate+ends_with loop — deleted
- `benches/response_parsing.rs:find_terminator_new()` — replaced with import
- `pipeline_worker.rs:has_terminator()` — deleted
- `fullduplex_worker.rs` (untracked, didn't compile) — deleted

---

### ❌ 2. Inline Error Kind Checks

**Never match on `ErrorKind` directly:**
```rust
// BAD: Will diverge across 10+ call sites
match e.kind() {
    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => { /* disconnect */ }
    _ => { /* other error */ }
}
```

**Use centralized helpers:**
```rust
use crate::connection_error::is_disconnect_kind;
if is_disconnect_kind(e.kind()) { /* ... */ }
```

---

### ❌ 3. Raw Byte Status Code Checks

**Never check status codes with `starts_with()`:**
```rust
if response.starts_with(b"430") { /* fragile */ }
```

**Use parsed status codes:**
```rust
if response.status_code == 430 { /* type-safe */ }
```

---

### ❌ 4. Wrong Buffer Pool Usage

**Never allocate scratch buffers in hot paths:**
```rust
// BAD: 1 allocation per request
let mut buf = vec![0u8; 8192];
stream.read(&mut buf).await?;
```

**Never call `extend_from_slice()` on a scratch buffer from `acquire()`:**
```rust
// BAD: acquire() buffer has len==724KB pre-set; extend grows beyond capacity,
// causes realloc, and breaks the pool debug_assert on return
let mut buf = buffer_pool.acquire().await;
buf.extend_from_slice(data); // ← WRONG MODE
```

**Never use `acquire_capture()` for single reads:**
```rust
// BAD: Capture buffers are single-use 768KB accumulators, not I/O scratch
let mut buf = buffer_pool.acquire_capture().await;
let n = buf.read_from(stream).await?; // ← WRONG MODE
```

**Correct usage:**
```rust
// For socket reads (scratch mode):
let mut buffer = buffer_pool.acquire().await;
let n = buffer.read_from(stream).await?;
let chunk = &buffer[..n]; // Only initialized portion

// For caching accumulation (capture mode, caching path only):
let mut captured = buffer_pool.acquire_capture().await;
// Then call streaming::stream_and_capture_multiline_response(...)
```

---

### ❌ 5. Duplicating Metrics Recording

**Never inline the `determine_metrics_action` match block:**
- Use `record_response_metrics()` (single source of truth)

---

### ❌ 6. Manual Struct Construction in Tests

**Never hardcode 17-field Server structs:**
```rust
// BAD: Hardcodes defaults, breaks on new fields
let server = Server { port: 8119, /* ... 16 more fields */ };
```

**Use builder pattern:**
```rust
let server = Server::builder().port(8119).build();
```

---

### ❌ 7. String Slicing Without UTF-8 Checks

**Never slice strings at byte offsets:**
```rust
// BAD: Panics on multi-byte chars
let truncated = &username[..9]; // Byte offset 9 might be mid-character
```

**Use character-based truncation:**
```rust
let truncated: String = username.chars().take(9).collect();
```

---

### ❌ 8. Benchmarks With Local Code Copies

**Never reimplement production functions in benchmarks:**
- Import the actual function
- Benchmark measures production code, not a copy

---

## Configuration Patterns

### CLI + Config File Pattern

**Precedence:** CLI args > config file > defaults

**ThreadCount example:**
```rust
let threads = args.threads
    .or(config.threads)
    .unwrap_or_else(|| ThreadCount::from_value(0).unwrap()); // Auto-detect
```

---

### Feature Flags

**Routing mode:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMode {
    Stateful,      // --routing-mode stateful
    PerCommand,    // --routing-mode per-command
    Hybrid,        // Default
}
```

**Cache backend:**
```rust
pub enum CacheBackend {
    Memory,        // moka LRU
    Disk,          // foyer hybrid (psync I/O)
    Both,          // 2-tier (memory + disk)
    None,          // Caching disabled
}
```

---

## File Organization

```
src/
├── bin/
│   ├── nntp-proxy.rs          # Main proxy binary
│   └── nntp-cache-proxy.rs    # Caching proxy binary
├── cache/                      # Cache implementations (moka, foyer)
├── command/                    # Command parsing + classification
├── config/                     # TOML config + CLI args
├── connection_error.rs         # Error types + classification
├── health/                     # Backend health checking
├── metrics/                    # Metrics recording + aggregation
├── pool/                       # Connection pool (deadpool wrapper)
│   ├── buffer.rs               # BufferPool (zero-alloc I/O)
│   ├── connection_guard.rs     # Pooled connection wrapper
│   └── prewarming.rs           # Pre-auth on startup
├── protocol/                   # RFC 3977 protocol types
│   ├── responses.rs            # Status codes + constants
│   └── validation.rs           # Response validation
├── proxy/                      # Main server loop + builder
├── router/                     # Routing logic (3 modes)
│   ├── backend_queue.rs        # Pipeline batching queue
│   └── backend_selector.rs     # Round-robin + health-aware selection
├── session/                    # Per-client session handling
│   ├── handlers/               # Command handlers (stateful, per-command, pipeline)
│   │   ├── article_retry.rs    # 430 retry logic
│   │   ├── command_execution.rs # Command dispatch
│   │   ├── per_command.rs      # PerCommand mode handler
│   │   ├── pipeline_worker.rs  # TCP pipelining worker
│   │   └── stateful.rs         # Stateful mode handler
│   ├── streaming/              # Response streaming
│   │   ├── tail_buffer.rs      # Terminator detection (SIMD)
│   │   └── mod.rs              # stream_multiline_response_impl
│   └── error_classification.rs # Error kind helpers
└── tui/                        # Terminal UI (ratatui)

tests/
├── auth/                       # Authentication tests
├── cache/                      # Cache behavior tests
├── proxy/
│   ├── routing/                # Routing mode tests
│   └── pipeline/               # TCP pipelining tests
└── test_helpers.rs             # Shared test utilities

benches/
├── command_parsing.rs          # Command classification benches
├── response_parsing.rs         # Terminator detection benches
└── router_selection.rs         # Routing logic benches
```

---

## Dependencies Worth Noting

**Core runtime:**
- `tokio` — async runtime (enables: `full`)
- `tikv-jemallocator` — allocator (better than system malloc for this workload)

**Protocol + I/O:**
- `rustls` + `tokio-rustls` — TLS (no OpenSSL dependency)
- `memchr` — SIMD-accelerated byte scanning (used in `TailBuffer`)

**Caching:**
- `moka` — async LRU memory cache
- `foyer` v0.22 — hybrid (memory + disk) cache
  - **Use psync I/O engine** (not io_uring, which has busy-poll bug)

**Connection pooling:**
- `deadpool` — async connection pool (wraps our backend connections)

**Concurrency primitives:**
- `crossbeam` — lock-free queue (used in `BackendQueue`)
- `parking_lot` — faster RwLock/Mutex (used in availability tracking)

**Metrics + observability:**
- `tracing` + `tracing-subscriber` — structured logging
- Custom `Metrics` type (lock-free counters)

**TUI:**
- `ratatui` + `crossterm` — terminal UI for monitoring

---

## Performance Notes from Profiling

**Hot path (62.7% CPU):**
- `stream_multiline_response_impl` — main streaming loop

**TLS overhead (30.64% CPU):**
- Backend reads (rustls decryption)

**Client writes (22.81% CPU):**
- Tokio socket writes to client

**Optimization techniques:**
- Double-buffering with `tokio::join!` to overlap read/write syscalls
- SIMD in terminator detection (memchr crate)
- Zero-copy streaming (no full buffering in proxy)

**Idle CPU bug (foyer io_uring):**
- io_uring engine spins at 53% CPU when idle (tight `try_recv` loop)
- **Solution:** Use psync I/O engine (default in foyer 0.22)

---

## Summary Checklist

Before submitting code, verify:

- [ ] No duplicate terminator detection (use `TailBuffer`)
- [ ] No scratch buffer allocations (use `BufferPool`)
- [ ] No inline error kind checks (use centralized helpers)
- [ ] No raw byte status code checks (use parsed `status_code`)
- [ ] No duplicate command classification (use classifier helpers)
- [ ] No duplicate metrics recording (use `record_response_metrics()`)
- [ ] Tests use `Server::builder()` (not manual struct construction)
- [ ] Benchmarks import production functions (no local copies)
- [ ] `cargo nextest run` passes (~2057 tests)
- [ ] `cargo clippy` clean
- [ ] `cargo fmt --check` clean
- [ ] For `[PERF]` changes: `cargo bench` shows no regression

---

## Questions or Clarifications?

**For architectural decisions not covered here:**
1. Check existing code in the relevant module
2. Look for similar patterns elsewhere in the codebase
3. Prefer reusing existing abstractions over creating new ones

**For protocol questions:**
- Consult `docs/RFC3977_RESPONSE_CODES.md`
- Check RFC 3977 specification

**For performance questions:**
- Profile with `cargo flamegraph` or `perf`
- Benchmark before/after with `cargo bench`
- Check `DEEP_DIVE_ANALYSIS.md` for past optimization notes

---

**Document version:** 2026-02-08
**Last updated:** Feature branch `feature/tcp-command-pipelining` refactoring
