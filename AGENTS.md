# Agent Rules For NNTP Proxy

This file is mandatory guidance for AI agents working in this repository.

## Workflow

- Run project commands through Nix: `nix develop -c <command>`.
- Use `rg`/`rg --files` for search.
- For normal code changes, run:
  - `nix develop -c cargo fmt --check`
  - `nix develop -c cargo clippy --all-features -- -D warnings`
  - `nix develop -c cargo nextest run`
- For performance-sensitive changes, benchmark before and after and accept no
  regressions.

## Architecture Snapshot

- Runtime: tokio async, rustls backend TLS, jemalloc allocator.
- Routing modes: hybrid, per-command, and stateful.
- Connection pooling: deadpool-backed, pre-authenticated backend connections.
- Caching: availability tracking plus optional memory/foyer hybrid payload cache.
- Buffers: `BufferPool::acquire()` for fixed scratch read buffers;
  `acquire_capture()` only for explicit full-response retention.

## Response Rules

- `src/session/multiline_framing.rs` is the only place that may know how NNTP
  multiline responses end.
- Do not inspect, compare, split, optimize, or test raw multiline boundaries
  outside the framer: CR/LF/dot bytes, chunk ends, suffixes, leftovers,
  terminator offsets, packed response ranges, `ends_with`, `starts_with`, or
  `windows`.
- If a response is incomplete, feed the next backend buffer back into the same
  `MultilineFramer`.
- Expose higher-level framer operations instead of caller-side boundary logic:
  write, capture, cache ingest, observe, or ordered response emission.
- Use `StatusCode::parse()` or already parsed status metadata. Do not check raw
  response bytes such as `response.starts_with(b"430")`.
- Response body expectations are request-scoped. Use
  `RequestContext::has_response_body()`; `211` is single-line for `GROUP` and
  multiline for `LISTGROUP`.

## Hot Path Ownership

- Normal ARTICLE/BODY/HEAD forwarding borrows packet bytes from the current
  pooled read buffer and writes those slices directly.
- Do not clone, freeze, detach, or copy pass-through ARTICLE/BODY/HEAD responses
  into `ChunkedResponse`, `Bytes`, `Vec`, or capture buffers.
- Full response ownership is acceptable only for payload caching, cache hits,
  list-style/XOVER/OVER/HDR paths that intentionally need owned data,
  tests/prechecks, and visible pool-exhaustion or oversized-retention fallbacks.

## Routing And Cache Semantics

- `ArticleAvailability` is a negative bitset for authoritative `430` facts.
  Record a backend missing only after it actually returns `430`.
- Preserve retry tiers: do not route ARTICLE/BODY/HEAD retries to a lower-priority
  tier until every eligible backend in the current tier has returned `430` for
  that request.
- Article cache keys are message IDs. STAT/HEAD/ARTICLE/BODY metadata and
  payload variants are distinct.
- The disk cache uses foyer hybrid with the psync I/O engine; do not switch it
  to io_uring.

## Error Handling

- Use `SessionError` in handler code.
- Treat `SessionError::ClientDisconnect(_)` as normal and do not log it as an
  error.
- Use `crate::connection_error::is_disconnect_kind` only in central raw
  `io::Error` conversion/classification code.
- `ConnectionGuard` removes dirty connections on error; explicitly release or
  complete connections only on success.

## Style

- Prefer enums over booleans for state.
- Use context structs instead of adding `#[allow(clippy::too_many_arguments)]`.
- Remove unused parameters instead of prefixing with `_`.
- Doc comments should explain why, not restate what code does.
- Never slice strings at byte offsets; use `chars().take(N).collect()` for safe
  truncation.
