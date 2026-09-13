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

The 2026-09-13 release-profile run on Tina (AMD Ryzen 9 5950X, 32 logical
CPUs) completed the stock cache-miss E2E matrix. It used the pinned nntpbench
revision from the harness, `RUSTFLAGS="-C target-cpu=native"`, a 10 GiB target
per cell, and the default 4 x 7 x 4 thread/connection/client matrix. The run
completed all 112 cells; summary and interpretation are in
[archive/release-benchmarks.md](archive/release-benchmarks.md).

The generated CSV remains on Tina at
`target/bench-results/release-cache-miss-e2e-20260913T190920Z.csv`; it is an
artifact of the run and is intentionally not committed.

The scanner benchmarks use an 8 MiB packed stream of eight 1 MiB multiline
responses. The Divan functions are explicit in
`benches/multiline_terminator_divan.rs`; the three scanning loops and fixture
are in the feature-gated `scanner_bench` module in
`src/session/multiline_framing.rs`. There are no custom benchmark macros.
Gungraun retains only the framework's required attributes and registration.

Build first, then record CPU and I/O activity. Prefer a quiet host; if running
under normal background load, report that condition and the repeated-run spread:

```bash
RUSTFLAGS="-C target-cpu=native" nix develop -c cargo bench \
  --features scanner-bench --bench multiline_terminator_divan \
  --bench multiline_terminator_gungraun --no-run
mpstat -P ALL 1 5
vmstat 1 5
```

Select an idle physical core and check its SMT sibling before timing.
For example, if CPU 4 and its sibling are idle:

```bash
RUSTFLAGS="-C target-cpu=native" nix develop -c taskset -c 4 cargo bench \
  --features scanner-bench --bench multiline_terminator_divan
GUNGRAUN_RUNNER=/home/mjc/.cargo/bin/gungraun-runner \
  RUSTFLAGS="-C target-cpu=native" nix develop -c cargo bench \
  --features scanner-bench --bench multiline_terminator_gungraun
```

Install Gungraun's runner once if needed:
`nix develop -c cargo install gungraun-runner --version 0.19.4 --locked`.

Divan flushes the input from every CPU cache level outside the timer before
each scan. **Keep sample size at one**; otherwise Divan's input batching can
turn this back into a warm-cache test. RAM eviction currently requires
x86_64 CLFLUSH and other architectures fail explicitly. Gungraun supports
Linux x86_64/aarch64 and reports instruction counts rather than RAM timing.
Neither benchmark generates or copies the fixture in the measured loop.

Repeat timing runs in alternating candidate order and retain the full
distribution, not just one median. Background memory traffic can change RAM
results even with CPU affinity. The
[audit record](archive/release-benchmarks.md) withdraws the earlier scanner
rankings and records the corrected six-pass RAM comparison under Tina's
normal background load. These are next-boundary
kernel comparisons; use the stock nntpbench E2E harness to evaluate an actual
production change.

The optional `scanner-bench` feature uses Ashwa 1.0.1 (Rust 1.89+); the default
package MSRV remains 1.88. Both targets require this feature, so normal
`cargo bench` builds do not import a disabled benchmark API.

When you want fresh numbers:

- microbenchmarks live under `benches/`
- end-to-end cache-miss benchmarking uses `scripts/bench-release-cache-miss-e2e.sh`
- profiling helpers include `scripts/parse_perfdata` and `scripts/parse_flamegraph`

Do not treat a single host run as a portable performance guarantee. Preserve
the harness defaults when comparing runs, and report the host, toolchain,
dataset target, and result artifact alongside any numbers.

## Manual smoke test

```bash
telnet localhost 8119
HELP
QUIT
```
