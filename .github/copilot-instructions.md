# NNTP Proxy AI Instructions

Use this as a short companion to `AGENTS.md`. Keep generated suggestions aligned
with the current code, not older architecture notes.

## Workflow

- Run commands through the project shell: `nix develop -c <command>`.
- Prefer `rg`/`rg --files` for searches.
- For normal code changes, verify with:
  - `nix develop -c cargo fmt --check`
  - `nix develop -c cargo clippy --all-features -- -D warnings`
  - `nix develop -c cargo nextest run`

## Current Architecture

- Runtime: tokio async, rustls backend TLS, jemalloc.
- Routing modes: hybrid, per-command, and stateful.
- Backend connections: deadpool-backed and pre-authenticated.
- Requests: parsed into `protocol::RequestContext`; do not reparse raw command
  strings at call sites.
- Response shape is request-scoped. Use `RequestContext::has_response_body()`;
  status code alone is not enough.

## Hard Rules

- Keep all NNTP multiline boundary logic in
  `src/session/multiline_framing.rs`.
- Do not scan for CR/LF/dot terminators, suffixes, chunk ends, or packed
  response ranges outside the framer.
- Normal ARTICLE/BODY/HEAD forwarding borrows bytes from pooled read buffers and
  writes those slices directly.
- Own full responses only for explicit retention paths: payload caching,
  cache-hit rendering, list-style responses that need ownership, tests/prechecks,
  and visible pool-exhaustion or oversized-retention fallbacks.
- `ArticleAvailability` is a negative bitset for authoritative `430` facts.
  Record missing only after a backend actually returns `430`.
- Preserve retry tier semantics: do not escalate to a lower-priority tier until
  every eligible backend in the current tier has returned `430` for that
  request.

## Where To Look

- Runtime and routing: `docs/operator/runtime-and-routing.md`
- Caching: `docs/operator/caching.md`
- Development workflow and response responsibilities: `docs/development.md`
- Response code reference: `docs/reference/rfc3977-response-codes.md`
- Tests helpers: `tests/test_helpers.rs`
