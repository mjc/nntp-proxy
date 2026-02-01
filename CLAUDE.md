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
           │    └─> PooledBuffer: single-read scratch (not accumulator)
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

**✅ ALWAYS use `TailBuffer::detect_terminator()`** for NNTP multiline terminator detection (`\r\n.\r\n`).

**❌ NEVER write:**
```rust
// BAD: Misses mid-chunk terminators
fn has_terminator(data: &[u8]) -> bool {
    data.ends_with(b"\r\n.\r\n")
}
```

**✅ DO write:**
```rust
use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

let mut tail = TailBuffer::default();
let status = tail.detect_terminator(chunk);
match status {
    TerminatorStatus::FoundAt(pos) => {
        let write_len = pos + 5; // Include terminator
        result.extend_from_slice(&chunk[..write_len]);
        // chunk[write_len..] is leftover for next response
    }
    _ => { /* continue reading */ }
}
```

**Why:**
- `ends_with()` only checks the **end** of a buffer — misses terminators in the **middle** (pipelined responses)
- `TailBuffer` handles: mid-chunk terminators, cross-boundary spanning, SIMD-accelerated scanning
- **Location:** `src/session/streaming/tail_buffer.rs` (30+ tests, production-proven)

**Pattern usage:**
- Streaming responses: `stream_multiline_response_impl`
- Pipeline worker: `read_full_response` (after commit 1)
- Cache validation: check if cached data is complete

---

### 2. I/O Buffer Management

**✅ ALWAYS use `BufferPool::acquire()`** for read scratch buffers in hot paths.

**❌ NEVER write:**
```rust
// BAD: Allocates per request
let mut scratch = vec![0u8; 8192];
stream.read(&mut scratch).await?;
```

**✅ DO write:**
```rust
use crate::pool::buffer::BufferPool;

let buffer_pool = BufferPool::default(); // or get from context
let mut buffer = buffer_pool.acquire().await;
let n = buffer.read_from(stream).await?;
let chunk = &buffer[..n];
// buffer automatically returned to pool on drop
```

**Why:**
- Eliminates per-request allocations (hot path optimization)
- Pool reuses 8KB buffers across requests
- **PooledBuffer is a single-read scratch buffer** — not an accumulator
- Use `read_from()` per chunk, don't try to accumulate multiple reads in one buffer

**Pattern usage:**
- Streaming: `stream_multiline_response_impl`
- Pipeline: `read_full_response`
- Any socket read in hot path

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
pub const MULTILINE_TERMINATOR: &[u8] = b"\r\n.\r\n"; // 5 bytes
pub const LINE_ENDING: &[u8] = b"\r\n";

/// Check if data ends with NNTP multiline terminator
#[inline]
pub fn has_multiline_terminator(data: &[u8]) -> bool {
    data.len() >= 5 && data.ends_with(MULTILINE_TERMINATOR)
}
```

**Note:** The 3-byte `.\r\n` check (without leading `\r\n`) is for **line-based readers** only — different semantics from the 5-byte terminator used for **chunk-based detection**.

**Why:**
- Magic bytes scattered across codebase will diverge
- Semantics documented at definition site
- Easy to grep for usage

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

### ❌ 1. Reimplementing Terminator Detection

**Never write your own `has_terminator()` or `find_terminator()`:**
- Use `TailBuffer::detect_terminator()` (SIMD-optimized, 30+ tests)
- Handles all edge cases (mid-chunk, spanning boundaries)

**Location of duplicates found during refactoring:**
- `pipeline_worker.rs:has_terminator()` — deleted
- `fullduplex_worker.rs` (untracked, didn't compile) — deleted
- `benches/response_parsing.rs:find_terminator_new()` — replaced with import

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

### ❌ 4. Allocating I/O Scratch Buffers

**Never allocate scratch buffers in hot paths:**
```rust
// BAD: 1 allocation per request
let mut buf = vec![0u8; 8192];
stream.read(&mut buf).await?;
```

**Use buffer pool:**
```rust
let mut buffer = buffer_pool.acquire().await;
let n = buffer.read_from(stream).await?;
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
