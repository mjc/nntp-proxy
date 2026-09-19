# Development

## Common commands

Build:

```bash
devenv shell cargo build
```

Format, lint, and test:

```bash
devenv shell cargo fmt --check
devenv shell cargo clippy --all-features -- -D warnings
devenv shell cargo nextest run
```

Use `cargo test` when you need doctests, exact filtering, or `-- --nocapture` debugging output.

Dependency and advisory triage:

```bash
devenv tasks run project:audit-advisories
```

See [security-advisories.md](security-advisories.md) for ignore policy and
revisit expectations.

## Quality checks

Entering the devenv shell installs the managed pre-commit hook. The hook runs
the `project:quality-fast` devenv task only when creating a commit; shell activation
itself does not run Clippy or any other checks. Run the quality gate explicitly
when needed:

```bash
devenv tasks run project:quality-fast
```

Run the full PR-equivalent quality gate with:

```bash
devenv tasks run project:quality-pr
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

Build cross-platform release artifacts with the pinned cross-compilation shell:

```bash
./scripts/build-release.sh <version>
```

The release script enters the flake's `cross` shell automatically. Use devenv
for local builds and quality checks; use the flake only for packaging and
cross-compilation.

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

### Response ownership contracts

`StreamingResponse` keeps the request's response shape, scanner, current window,
backend and pool together inside `multiline_framing.rs`. Write, observe and
capture consume this operation. `IsolatedMultilineResponse` owns the same
continuation relationship but rejects packed suffixes. Neither operation lends
an independently usable continuation to a handler.

The scanner returns `ChunkConsumed`, a count relative to the latest push.
`FrameEnd` is an exclusive position in the current logical response window,
not its physical allocation. Translation occurs inside the framer, including
after compaction. Coordinates do not establish buffer identity: the
operation's exclusive borrow and the storage-owned `AppendedRead` establish
that association.

Storage does not classify responses. `RetainedAppendPermit::read` consumes one
permission and returns either EOF or an append result bound to the same buffer.
Logical exhaustion cannot create a permit; it is not EOF. Compaction policy and
pooled allocation reuse are unchanged. Ordinary forwarding borrows current
pooled bytes; only intentional capture/cache paths retain entire responses.

nntpbench uses the same framing-versus-validation distinction and consuming
mutable operations, but freezes completed prefixes into immutable owners.
Its typed article accessor reuses validated layout and transformation metadata;
proxy pass-through does not manufacture that semantic guarantee.

Cancellation may preserve bytes but does not prove a completed exchange. An
unreleased `ConnectionGuard` retires its connection on drop in every build mode.

Explicit compiler-contract checks exercise real private types, with successful
controls and checked diagnostic codes:

```bash
nix develop -c bash scripts/check-response-contracts.sh
```

### Measurement

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
