#!/usr/bin/env bash
# PR-equivalent checks for local use and Codex app automations.

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

run() {
    echo
    echo "==> $*"
    "$@"
}

run scripts/quality-fast.sh
run cargo llvm-cov nextest --workspace --profile ci --lcov --output-path lcov.info
run cargo deny check
run cargo audit
run cargo shear

if [ -d supply-chain ]; then
    run cargo vet
else
    echo
    echo "==> cargo vet"
    echo "skipped: supply-chain/ is not initialized yet"
fi

echo
echo "quality-pr passed"
