# Agent Rules For NNTP Proxy

This file is mandatory guidance for AI agents working in this repository.

Use the project shell for commands:

```sh
nix develop -c <command>
```

## Architecture Snapshot

- Runtime: tokio async, rustls TLS, jemalloc allocator.
- Routing modes: stateful, per-command, and hybrid.
- Connection pooling: deadpool-backed, pre-authenticated backend connections.
- Caching: memory cache plus foyer hybrid disk cache.
- Buffer management: fixed scratch buffers for socket reads and capture buffers
  only for explicit full-response retention.

`run_stateful_proxy_loop` takes generic `W: AsyncWrite + Unpin`; after auth
responses and backend writes, call `.flush().await?`. `handle_per_command_routing`
uses concrete `WriteHalf<'_>`; changing that cascades through many signatures.

## Do Not Touch Multiline Boundaries Outside The Framer

`src/session/multiline_framing.rs` is the only place allowed to know how NNTP
multiline responses end.

Do not inspect, compare, split, optimize, or reason about any of these outside
that module:

- `\r\n.\r\n`
- `.\r\n`
- `\r`
- `\n`
- chunk ends
- string ends
- suffixes
- leftovers
- terminator offsets
- packed response byte ranges

After code calls `MultilineFramer`, the response is either handled by
framer-owned operations or the same `MultilineFramer` receives the next backend
buffer. Do not add caller-side split state, offset math, `ends_with`,
`starts_with`, `windows`, manual CR/LF checks, or fast paths.

Forbidden outside `src/session/multiline_framing.rs`:

```rust
data.ends_with(b"\r\n.\r\n")
data.ends_with(b".\r\n")
line == b".\r\n"
line.starts_with(b".")
chunk.windows(...).any(...)
let response = &buf[..end];
let suffix = &buf[end..];
```

Do not add constants such as `MULTILINE_TERMINATOR`, helper functions such as
`has_multiline_terminator()`, local benchmark scanners, or tests outside the
framer that assert raw terminator offsets, chunk-boundary offsets, or suffix
ranges.

If a change seems to need any of that, the design is wrong. Move the behavior
into `MultilineFramer` and expose a higher-level operation such as write,
capture, cache ingest, or ordered response emission.

## Borrow Packet Data On Proxy Hot Paths

The normal forwarding path must borrow packet/response bytes from the current
pooled read buffer and write those borrowed slices directly. Do not own, clone,
freeze, detach, copy into `ChunkedResponse`, `Bytes`, `Vec`, or capture buffers
on the pass-through ARTICLE/BODY/HEAD path.

Owning a full response is only acceptable for explicit retention paths:

- article/body/head payload caching
- cache hits serving already-owned payloads
- list-style/XOVER/OVER/HDR paths that intentionally need an owned response
- tests or precheck paths that explicitly require ownership
- pool exhaustion fallbacks, which must remain visible in metrics

If a change wants to keep packet data after the current write, treat that as a
design smell. Prefer zero-copy or borrow-only handoff from framer-owned typed
operations. Release or reuse pooled buffers as soon as their borrowed bytes have
been written.

## Buffer Pool Modes

`BufferPool` has two distinct modes. Using the wrong one causes silent
reallocation and/or corrupted data.

### Scratch Buffers

Use `acquire()` for socket reads. These buffers are pre-allocated fixed-size
vectors with `len == capacity`.

```rust
let mut buffer = buffer_pool.acquire().await;
let n = buffer.read_from(stream).await?;
let chunk = &buffer[..n];
```

Do not call `extend_from_slice` on scratch buffers, and do not allocate local
scratch buffers in hot paths.

### Capture Buffers

Use `acquire_capture()` only when a retention path explicitly needs full-response
ownership, such as `CacheAction::CaptureArticle`. Capture buffers start empty
with pre-allocated capacity and may be extended.

Do not use capture buffers for socket-read scratch space.

## Preserve Retry Tier Semantics

Do not route ARTICLE/BODY/HEAD retry attempts to a lower-priority tier just
because the preferred tier is saturated, has waiters, or looks slower in a
profile. Tier escalation is only allowed after every eligible backend in the
current tier has actually returned the article-missing response for that request
and has been recorded missing in `ArticleAvailability`.

Throughput work on the retry path must preserve that invariant. Optimize
connection reuse, buffering, lock lifetime, request pipelining, and selection
inside the current eligible tier; do not use pool pressure as a reason to skip
ahead to tier 1+ before tier 0 has all 430s.

## Error Handling

Use `SessionError` in handler code. Client disconnects are encoded as enum
variants, not detected through ad hoc downcasts or inline `io::ErrorKind`
matches.

```rust
match result {
    Ok(v) => { /* success */ }
    Err(SessionError::ClientDisconnect(_)) => { /* normal, do not log */ }
    Err(SessionError::Backend(e)) => { warn!("{e}"); }
}

Err(e @ SessionError::ClientDisconnect(_)) => return Err(e),
```

Use `crate::connection_error::is_disconnect_kind` for raw `io::Error` only in
central conversion/classification code.

`StreamingError` communicates pool fate. Use `must_remove_connection()` where
that decision is needed. `ConnectionGuard` removes dirty connections on error;
explicitly call `guard.release()` only on success.

Protocol errors should return NNTP error responses to the client unless the
condition is fatal.

## Status Codes, Commands, And Metrics

Use pre-parsed status code fields or `StatusCode::parse()`. Do not check raw
bytes with patterns such as `response.starts_with(b"430")`.

Use command classifier helpers from `src/command/classifier.rs`, such as
`is_large_transfer_command`, `is_stat_command`, and `is_head_command`. Do not
duplicate command parsing with local `memchr` and match tables.

Use `record_response_metrics()` for the `determine_metrics_action` pattern. Do
not inline the metrics match block at call sites.

## Caching

Article cache keys are message IDs; STAT/HEAD/ARTICLE/BODY variants are cached
separately.

The disk cache uses foyer hybrid with the psync I/O engine. Do not switch it to
io_uring.

Foyer disk cache tests hang in the test harness; keep those tests ignored and
test manually when needed.

## Metrics Persistence

`MetricsStore` persists cumulative dashboard counters to versioned JSON and is
loaded on startup. Live gauges such as active connections, stateful sessions,
backend active connections, backend health, and process start time are not
persisted.

When adding persisted metrics, migrate the stats file version and provide zero
defaults for missing fields.

## Testing And Benchmarks

Use `Server::builder()` in tests; do not hardcode large `Server` struct
literals. Use `..Default::default()` for large test structs.

When wrapping writers in `BufWriter`, call `.flush().await?` after auth
responses and backend-to-client writes, or integration tests can time out.

Use helpers from `tests/test_helpers.rs` instead of duplicating per-file helper
functions.

Benchmarks must import and drive production code. Do not define local
reimplementations of production framing, classification, or parsing logic.

Run these checks before submitting normal code changes:

```sh
nix develop -c cargo nextest run
nix develop -c cargo clippy
nix develop -c cargo fmt --check
```

For performance-sensitive changes, run the relevant benchmarks before and after
the change and accept no regressions.

## Perf Data Parsing

When inspecting `perf.data`, prefer the repo helper:

```sh
nix develop -c scripts/parse_perfdata perf.data
```

It reads `perf.data` directly and emits the thread, instruction-pointer, edge,
timeline, and category summaries agents usually need for profile triage. Do not
replace it with a text-rendered parser; the point of this helper is to avoid
waiting on perf's renderer for large callchains.

## Code Style

Prefer enums over booleans for state. Extract repeated 5+ line blocks into
methods. Use context structs for related parameters instead of adding
`#[allow(clippy::too_many_arguments)]`.

Remove unused parameters instead of prefixing them with `_`.

Doc comments should explain why, not restate what the code does. Link to RFCs
for protocol features. Use inline comments only when logic is not self-evident.

Never slice strings at byte offsets; use `chars().take(N).collect()` for safe
truncation.
