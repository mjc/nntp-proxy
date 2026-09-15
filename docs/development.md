# Development

## Common commands

Build:

```bash
devenv shell -- cargo build
```

Format, lint, and test:

```bash
devenv shell -- cargo fmt --check
devenv shell -- cargo clippy --all-features -- -D warnings
devenv shell -- cargo nextest run
```

Use `cargo test` when you need doctests, exact filtering, or `-- --nocapture` debugging output.

Dependency and advisory triage:

```bash
devenv shell -- scripts/audit-advisories
```

See [security-advisories.md](security-advisories.md) for ignore policy and
revisit expectations.

## Quality checks

The devenv shell does not install or run Git hooks during activation. Run the
quality gate explicitly when needed:

```bash
devenv shell -- scripts/quality-fast.sh
```

## Nix

The local development environment is managed by devenv:

```bash
devenv shell
```

The existing Nix flake remains the packaging and cross-compilation interface.

Build the packaged binary with Nix:

```bash
nix build .#default
```

## Response Handling Work

Use RFC 3977 and RFC 4643 when protocol behavior is unclear. In this codebase,
response shape is request-scoped: check `RequestContext::has_response_body()`
instead of inferring multiline behavior from a status code alone.

Keep multiline response boundary logic in `src/session/multiline_framing.rs`.
Benchmarks and tests should import production framing and request-classification
code instead of reimplementing terminator scanners or local command parsers.

See [AGENTS.md](../AGENTS.md) for the canonical repository rules.

Current response responsibilities:

- `src/protocol/request.rs` owns `RequestContext`, route classes, response-body
  expectations, and request-scoped response/cache metadata.
- `src/protocol/response.rs` owns three-digit status parsing.
- `src/protocol/responses.rs` owns local single-line response constants and
  small formatting helpers.
- `src/session/multiline_framing.rs` owns all response-boundary detection,
  packed-next-response preservation, ordered response emission, capture,
  observe, and write operations.
- `src/session/backend.rs` is the facade over backend execution and
  framer-owned operations.
- `src/session/response_transfer.rs` maps transfer outcomes to backend
  connection reuse decisions.
- `src/cache/article.rs` and `src/cache/hybrid_codec.rs` store semantic article
  payload sections and render cache-hit responses back to NNTP wire bytes.

## Benchmarks

Published benchmark numbers were intentionally removed from the docs until they are rerun.

When you want fresh numbers:

- microbenchmarks live under `benches/`
- end-to-end cache-miss benchmarking uses `scripts/bench-release-cache-miss-e2e.sh`
- profiling helpers include `scripts/parse_perfdata` and `scripts/parse_flamegraph`

Do not treat old README benchmark values as current project guarantees.

## Manual smoke test

```bash
telnet localhost 8119
HELP
QUIT
```
