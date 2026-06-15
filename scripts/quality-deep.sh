#!/usr/bin/env bash
# Automation-friendly deep checks. Run with: scripts/quality-deep.sh <suite>

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

suite="${1:-}"

run() {
    echo
    echo "==> $*"
    "$@"
}

usage() {
    cat <<'EOF'
usage: scripts/quality-deep.sh <suite>

suites:
  features  cargo-hack feature checks
  deps      dependency, advisory, and supply-chain checks
  mutants   focused mutation testing
  benches   targeted performance benchmarks
  ub        focused undefined-behavior checks
  all       run all deep suites
EOF
}

features() {
    run cargo hack check --each-feature --all-targets
}

deps() {
    run scripts/audit-advisories
    run cargo shear
    if [ -d supply-chain ]; then
        run cargo vet
    else
        echo
        echo "==> cargo vet"
        echo "skipped: supply-chain/ is not initialized yet"
    fi
}

mutants() {
    run cargo mutants \
        --timeout 300 \
        --in-place false \
        --file src/session/routing/cache_policy.rs \
        --file src/session/routing/metrics_policy.rs \
        --file src/session/handlers/article_retry.rs \
        --file src/session/multiline_framing.rs
}

benches() {
    run cargo bench --bench response_parsing
    run cargo bench --bench command_parsing
    run cargo bench --bench cache_ingest
    run cargo bench --bench cache_lookup
    run cargo bench --bench cache_response_generation
}

ub() {
    if command -v cargo-careful >/dev/null 2>&1; then
        run cargo careful test --lib
    else
        echo "cargo-careful is not available in this shell" >&2
        return 1
    fi
}

case "$suite" in
    features) features ;;
    deps) deps ;;
    mutants) mutants ;;
    benches) benches ;;
    ub) ub ;;
    all)
        features
        deps
        mutants
        benches
        ub
        ;;
    -h|--help|"")
        usage
        [ -n "$suite" ]
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
