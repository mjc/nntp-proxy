# Development

## Common commands

Build:

```bash
cargo build
```

Format, lint, and test:

```bash
cargo fmt --check
cargo clippy --all-features -- -D warnings
cargo nextest run
```

Use `cargo test` when you need doctests, exact filtering, or `-- --nocapture` debugging output.

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
