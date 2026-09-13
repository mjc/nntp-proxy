# Development

## Common commands

Build:

```bash
nix develop -c cargo build
```

Format, lint, and test:

```bash
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-features -- -D warnings
nix develop -c cargo nextest run
```

Use `cargo test` when you need doctests, exact filtering, or `-- --nocapture` debugging output.

Dependency and advisory triage:

```bash
nix develop -c scripts/audit-advisories
```

See [security-advisories.md](security-advisories.md) for ignore policy and
revisit expectations.

## Pre-commit hook

The pre-commit hook runs `cargo fmt --check` and `cargo clippy --all-features -- -D warnings`. Install it with:

```bash
./scripts/install-git-hooks.sh
```

If Nix is available, the hook re-enters the dev shell automatically for consistent tooling.

## Nix

If you use the flake/dev shell:

```bash
nix develop
```

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

AI-facing repository rules are intentionally short. See [AGENTS.md](../AGENTS.md)
and [.github/copilot-instructions.md](../.github/copilot-instructions.md).

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

The stock cache-miss E2E harness ran on an AMD Ryzen 9 5950X with 32 logical
CPUs. `main` commit `145b36c6b86621dccfe965926b0ef501ffd33b06` and Finder
candidate commit `e176fef31432ed36dc101471dfa1c94cc1650908` each completed
the default 112-cell matrix twice for Finder (10 GiB per cell, 728,320-byte
articles, article-only mix, pipeline depth 32). The fresh main control's
equal-weight mean was 3,546.6 MiB/s; Finder repeats were 3,564.3 MiB/s
(+0.5%) and 3,560.8 MiB/s (+0.4%). Their paired medians were -0.4% and
+0.4%, geometric means +0.6% and -0.5%, and win/loss counts 50/62 and 63/49.
Measured proxy CPU changed -0.8% then +0.2%. The fresh main control was 0.8%
above the older main baseline. The repeats have substantial per-cell spread,
so they do not demonstrate a reliable end-to-end speedup. The fresh CSVs are
retained locally and are not committed.

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
