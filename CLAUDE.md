# NNTP Proxy — Development Guidelines

> **Purpose:** Canonical reference for codebase patterns, architectural decisions, and mandatory conventions.
> **Audience:** AI assistants (Claude Code), human contributors, code reviewers.
> **Principle:** Reuse > Reimplementation. When a pattern exists, use it — don't duplicate it.

---

## Architectural Overview

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
           │    ├─> PooledBuffer [acquire()]: single-read I/O scratch (724KB, pre-faulted)
           │    └─> PooledBuffer [acquire_capture()]: accumulator for caching (768KB, pre-faulted)
           │
           └──> Pipeline Engine
                ├─> BackendQueue: batched ARTICLE/BODY requests
                └─> Batching: 4-16 commands per round-trip
```

### Routing Modes

1. **Stateful** (1:1 client↔backend mapping) — GROUP, XOVER, article-by-number commands
2. **PerCommand** (stateless, shared pool) — message-ID based operations only
3. **Hybrid** (default) — starts PerCommand, auto-switches to Stateful on GROUP/XOVER/etc.

**Important:** `run_stateful_proxy_loop` takes generic `W: AsyncWrite + Unpin` — **MUST** call `.flush().await?` after auth responses and backend writes. `handle_per_command_routing` uses concrete `WriteHalf<'_>` — changing to generic cascades through many signatures.

---

## Mandatory Patterns

### 1. Terminator Detection

**`TailBuffer` is the ONE AND ONLY place terminator detection can exist in this codebase. There must never be any other implementation anywhere.**

An NNTP multiline response ends with `\r\n.\r\n` (5 bytes). Detecting this correctly across async chunk reads is subtle and easy to get wrong. Getting it wrong causes **connection pool collapse**.

#### Forbidden Patterns — Never Write These

```rust
// ❌ ends_with check on accumulated buffer
if data.ends_with(b"\r\n.\r\n") { ... }
if data.ends_with(b".\r\n") { ... }

// ❌ Any helper wrapping ends_with
fn has_terminator(data: &[u8]) -> bool { data.ends_with(b"\r\n.\r\n") }

// ❌ Constant replicated outside tail_buffer.rs
const MULTILINE_TERMINATOR: &[u8] = b"\r\n.\r\n";

// ❌ Accumulate-then-check loop (the worst pattern)
let mut accumulated = Vec::new();
loop {
    accumulated.extend_from_slice(&buf[..n]);
    if accumulated.ends_with(b"\r\n.\r\n") { break; } // WRONG
}
```

#### Correct: Streaming (most common case)

```rust
use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};

let mut tail = TailBuffer::default();
loop {
    let n = stream.read(buffer.as_mut_slice()).await?;
    if n == 0 { break; }
    let chunk = &buffer[..n];

    match tail.detect_terminator(chunk) {
        TerminatorStatus::FoundAt(pos) => {
            // CRITICAL: pos points to '\r' in "\r\n.\r\n"
            // Do NOT write chunk[pos..] — that's the start of the next response
            client.write_all(&chunk[..pos]).await?;
            break;
        }
        TerminatorStatus::NotFound => {
            client.write_all(chunk).await?;
            tail.update(chunk);
        }
    }
}
```

#### Correct: Complete-buffer validation

```rust
// TailBuffer with no prior state, checking a fully-accumulated buffer
TailBuffer::default().detect_terminator(&buffer).is_found()
```

#### Why `ends_with()` Causes Connection Pool Collapse

This caused a production bug (20 connections removed in rapid succession, pool exhausted):

1. Backend sends article body + start of next pipelined response in the same TCP segment
2. `read()` returns `[...article data...\r\n.\r\n200 Article follows\r\n...]`
3. `ends_with(b"\r\n.\r\n")` returns **false** — buffer ends with `...200 Article follows\r\n...`
4. Loop continues, consuming the `220 0 <next-msg-id>\r\n` for the next pipelined command
5. Next handler reads garbage → `Invalid` → `remove_with_cooldown()` → cascade to zero

`TailBuffer::detect_terminator()` returns `FoundAt(pos)` at the correct boundary — pipelined bytes are never consumed from the wrong context.

**Location:** `src/session/streaming/tail_buffer.rs` (30+ tests, production-proven)

---

### 2. I/O Buffer Management

`BufferPool` has **two distinct modes**. Using the wrong one causes silent reallocation and/or corrupted data.

#### Mode 1: Scratch buffers — `acquire()`

Pre-allocated fixed-size `Vec<u8>` with `len == capacity` (724KB, page-faulted). For single read operations.

```rust
// ✅ ALWAYS use for socket reads:
let mut buffer = buffer_pool.acquire().await;
let n = buffer.read_from(stream).await?;
let chunk = &buffer[..n]; // Only initialized portion

// ❌ NEVER extend_from_slice — buffer is len=724KB already; extend grows beyond capacity,
// causes realloc, and breaks the pool debug_assert on return
buf.extend_from_slice(data); // WRONG

// ❌ NEVER allocate scratch in hot paths:
let mut scratch = vec![0u8; 8192]; // WRONG: 1 allocation per request
```

#### Mode 2: Capture buffers — `acquire_capture()`

Pre-faulted empty `Vec<u8>` with `len == 0, capacity == 768KB`. For accumulating full streaming responses (caching path only).

```rust
// ✅ ONLY use under CacheAction::CaptureArticle:
let mut captured = self.buffer_pool.acquire_capture().await;
streaming::stream_and_capture_multiline_response(backend, client, first_chunk, ..., &mut captured).await?;
self.maybe_cache_upsert(msg_id, &captured, backend_id);

// ❌ NEVER use acquire_capture() for single reads:
let n = buf.read_from(stream).await?; // WRONG MODE
```

| Need | Method | Buffer state | `extend_from_slice`? |
|---|---|---|---|
| Socket read | `acquire()` | len=724KB | ❌ Never |
| Cache accumulate | `acquire_capture()` | len=0, cap=768KB | ✅ Correct |

**Location:** `src/pool/buffer.rs`

---

### 3. Error Classification

**✅ ALWAYS use centralized error kind constants** from `connection_error.rs`.

```rust
// ❌ NEVER inline — will diverge across call sites:
match e.kind() {
    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => true,
    _ => false,
}

// ✅ For raw io::Error:
use crate::connection_error::is_disconnect_kind;
if is_disconnect_kind(e.kind()) { /* client gone */ }

// ✅ For anyhow::Error (common in handlers):
use crate::is_client_disconnect_error; // re-exported from proxy/mod.rs
if is_client_disconnect_error(&e) { /* client disconnected */ }

// ✅ For StreamingError (returned by streaming functions):
if streaming_err.is_client_disconnect() { /* client disconnected, backend clean */ }
```

**Canonical definitions** (`src/connection_error.rs`):
- `DISCONNECT_KINDS`: `[BrokenPipe, ConnectionReset]` — client gone, don't log as backend failure
- `CONNECTION_ERROR_KINDS`: `[BrokenPipe, ConnectionReset, ConnectionAborted, UnexpectedEof]` — don't return to pool

**`StreamingError`** (`src/session/streaming/mod.rs`): Typed return from all streaming functions.
- `ClientDisconnect(io::Error)` — backend was cleanly drained; **return connection to pool**
- `BackendDirty(anyhow::Error)` — drain failed; **remove from pool**
- `BackendEof { .. }` — backend closed before terminator; **remove from pool**
- `Io(anyhow::Error)` — other error; **remove from pool**
- Use `e.must_remove_connection()` to decide pool fate; `e.into_anyhow()` to propagate as `anyhow::Error` (preserves `io::Error` root cause for `is_client_disconnect_error()` compatibility)

---

### 4. Status Code Checks

**✅ ALWAYS use pre-parsed `status_code` fields or `StatusCode::parse()`.**

```rust
// ❌ NEVER — fragile, false-positives on "4300", "430 text", etc.:
if response.data.starts_with(b"430") { ... }

// ✅ Use typed status codes:
if response.status_code == 430 { ... }
if cmd_response.is_430() { ... }
```

---

### 5. Command Classification

**✅ ALWAYS use classifier helpers** from `src/command/classifier.rs`.

```rust
// ❌ NEVER inline — the memchr + matches_any pattern was duplicated 4 times:
let end = memchr(b' ', cmd).unwrap_or(cmd.len());
if end >= 4 && matches_any(&cmd[..end], ARTICLE_CASES) { ... }

// ✅ Use the helpers (all #[inline(always)]):
use crate::command::classifier::{is_large_transfer_command, is_stat_command, is_head_command};
if is_large_transfer_command(cmd) { /* ARTICLE/BODY — route to batch pipeline */ }
```

---

### 6. Metrics Recording

**✅ ALWAYS use `record_response_metrics()`** for the `determine_metrics_action` pattern — never inline the match block. Centralized in `src/session/handlers/command_execution.rs`.

---

### 7. Protocol Constants

**Available in `src/protocol/responses.rs`:** `CRLF`, `TERMINATOR_TAIL_SIZE`

**Do not add `MULTILINE_TERMINATOR` or `has_multiline_terminator()`** — these were deleted because they encouraged bypassing `TailBuffer`, causing pool collapse. Use `TailBuffer` (see Pattern #1).

---

## Caching

**Two-tier cache:**
- Memory: moka (LRU) or foyer-memory, configured via `memory_cache_capacity`
- Disk: foyer hybrid with **psync I/O engine** — ❌ **DO NOT use io_uring** (53% idle CPU bug from tight `try_recv` loop)

**Cache key:** Message-ID; STAT/HEAD/ARTICLE/BODY variants cached separately.

**foyer disk cache tests** hang in the test harness — mark `#[ignore]`, test manually.

---

## Metrics Persistence

**Why:** TUI dashboard metrics reset on proxy restart. `MetricsStore` persists cumulative counters to JSON (30-second interval + on shutdown), restored on startup.

**Architecture:**
```
MetricsCollector
└── inner: Arc<MetricsInner>
    ├── store: MetricsStore         ← persisted (save/load JSON)
    │   ├── total_connections: AtomicU64
    │   ├── backend_stores: Vec<BackendStore>
    │   ├── pipeline_*: AtomicU64
    │   └── user_metrics: DashMap<String, UserMetrics>
    ├── active_connections: AtomicUsize  ← live gauge, NOT persisted
    ├── stateful_sessions: AtomicUsize   ← live gauge, NOT persisted
    └── start_time: Instant              ← NOT persisted

BackendMetrics
├── (no store field — writes go directly to MetricsStore.backend_stores[idx])
├── active_connections: AtomicUsize  ← live gauge
└── health_status: BackendHealthStatus  ← live gauge
```

**File format** (`stats.json`): versioned JSON with `"version": 1`. `MetricsStore::load()` handles migration. Add new fields in v2 with zero defaults.

**Integration:** Both binaries call `load_metrics_from_disk()` at startup, `save_to_disk()` on interval and shutdown. Config: `stats_file` in `[proxy]` section (defaults to `stats.json` alongside config).

**Tests:** `src/metrics/store.rs` — roundtrip, name-mapping, missing file, corruption, backend mismatch.

---

## Error Handling

### Error Types

- **`ConnectionError`** (`src/connection_error.rs`): wraps I/O/TLS errors, protocol violations
- **`StreamingError`** (`src/session/streaming/mod.rs`): typed outcome of streaming operations; use `must_remove_connection()` to decide pool fate (see Pattern #3)
- **`anyhow::Error`** (handlers): general propagation; check with `is_client_disconnect_error()`
- **Protocol errors**: return NNTP error response to client, don't terminate unless fatal

### Connection Pool Discipline

On streaming error, the pool fate depends on whether the backend was cleanly drained:
- `ClientDisconnect` → `drop(conn)` (returns to pool — backend is clean)
- All other `StreamingError` variants → `provider.remove_with_cooldown(conn)` (connection state unknown)

Always add a comment at each `remove_with_cooldown` call site explaining why a disconnect check is/isn't needed.

### Panic Safety

- `src/pool/buffer.rs` uses `MaybeUninit` — no other unsafe blocks
- **Never slice strings at byte offsets**: use `chars().take(N).collect()` for truncation

---

## Testing

### Key Patterns

**Use `Server::builder()` — never hardcode struct literals:**
```rust
// ❌ BAD: hardcodes defaults, breaks on new fields
let server = Server { port: 8119, pipeline_batch_size: 16, /* ... 15 more */ };

// ✅ GOOD: tracks production defaults automatically
let server = Server::builder().port(8119).build();
```

**Use `..Default::default()` for large structs:**
```rust
let snapshot = MetricsSnapshot { cache_hits: 10, ..Default::default() };
```

**BufWriter + auth tests:** When wrapping writers in `BufWriter`, **MUST** call `.flush().await?` after auth responses AND backend→client writes, or integration tests timeout.

**Shared test helpers:** Use `tests/test_helpers.rs` — never duplicate helper functions per test file.

### Benchmarks

**Never define local reimplementations — always import production functions:**
```rust
// ❌ BAD: benchmark measures wrong code
fn find_terminator_new(data: &[u8]) -> Option<usize> { ... }

// ✅ GOOD:
use nntp_proxy::session::streaming::tail_buffer::find_terminator_end;
```

**Performance testing:** Run `cargo bench` before and after performance-sensitive changes; accept no regressions.

**Profiling scripts:** `scripts/parse_perfdata` (hotspot analysis) and `scripts/parse_flamegraph` (SVG flamegraph). Usage: `perf script 2>/dev/null | ./scripts/parse_perfdata`.

---

## Code Style

**Enums over booleans for state** — compiler enforces exhaustive matching, self-documenting:
```rust
// ❌ BAD: bool flags
// ✅ GOOD:
enum SingleCommandResult { Continue { auth_succeeded: bool }, Quit(u64), SwitchToStateful }
```

**Extract methods for repeated 5+ line blocks** — when you see the same logic twice, extract it.

**Context structs over many arguments** — use a struct to group related parameters instead of `#[allow(clippy::too_many_arguments)]`.

**Remove unused parameters** — don't use `_param` prefix; just remove them. Compiler enforces no future accidental use.

**Doc comments:** explain *why*, not *what*. Link to RFCs for protocol features. Inline comments only where logic isn't self-evident.

---

## Anti-Patterns (Don't Do This)

### ❌ 1. Any Terminator Detection Outside TailBuffer

Any occurrence of the following anywhere outside `tail_buffer.rs` is a bug:
- `ends_with(b"\r\n.\r\n")` or `ends_with(b".\r\n")`
- `MULTILINE_TERMINATOR` or `TERMINATOR` constants
- `has_multiline_terminator()` or similar helper functions
- Any manual byte loop scanning for `'.'` or `'\r'` to find the terminator
- Accumulate-into-Vec + ends_with loop pattern

This caused real connection pool collapse in production. See Pattern #1 for the full explanation.

### ❌ 2. Inline Error Kind Checks

```rust
// BAD: will diverge across call sites
match e.kind() { ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => ... }

// GOOD:
use crate::connection_error::is_disconnect_kind;
if is_disconnect_kind(e.kind()) { ... }
```

### ❌ 3. Raw Byte Status Code Checks

```rust
if response.starts_with(b"430") { ... }  // BAD: fragile
if response.status_code == 430 { ... }   // GOOD: type-safe
```

### ❌ 4. Wrong Buffer Pool Usage

```rust
let mut buf = buffer_pool.acquire().await;
buf.extend_from_slice(data);              // BAD: wrong mode, causes realloc
let mut buf = vec![0u8; 8192];           // BAD: allocates per request

let mut buf = buffer_pool.acquire_capture().await;
let n = buf.read_from(stream).await?;    // BAD: wrong mode for I/O scratch
```

### ❌ 5. Duplicating Metrics Recording

Never inline the `determine_metrics_action` match block. Use `record_response_metrics()`.

### ❌ 6. Manual Struct Construction in Tests

```rust
let server = Server { port: 8119, /* ... 16 more fields */ }; // BAD
let server = Server::builder().port(8119).build();             // GOOD
```

### ❌ 7. String Slicing at Byte Offsets

```rust
let truncated = &username[..9];                         // BAD: panics on multi-byte chars
let truncated: String = username.chars().take(9).collect(); // GOOD
```

### ❌ 8. Benchmarks With Local Code Copies

Never reimplement production functions in benchmarks — import them directly.

---

## Summary Checklist

Before submitting code:

- [ ] No duplicate terminator detection (use `TailBuffer`)
- [ ] No scratch buffer allocations (use `BufferPool`)
- [ ] No inline error kind checks (use centralized helpers)
- [ ] No raw byte status code checks (use parsed `status_code`)
- [ ] No duplicate command classification (use classifier helpers)
- [ ] No duplicate metrics recording (use `record_response_metrics()`)
- [ ] Tests use `Server::builder()` (not manual struct construction)
- [ ] Benchmarks import production functions (no local copies)
- [ ] `cargo nextest run` passes
- [ ] `cargo clippy` clean
- [ ] `cargo fmt --check` clean
- [ ] For performance changes: `cargo bench` shows no regression
