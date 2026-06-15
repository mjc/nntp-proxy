#!/usr/bin/env bash
# Fast checks suitable for pre-commit hooks and Codex app quick automations.

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

run() {
    echo
    echo "==> $*"
    "$@"
}

mapfile -t shell_scripts < <(find scripts -maxdepth 1 -type f \( -name '*.sh' -o -perm -111 \) | sort)

run cargo fmt --check
run cargo clippy --all-targets --all-features -- -D warnings
run shellcheck -S warning "${shell_scripts[@]}"
run scripts/check-guardrails.sh
run actionlint
run zizmor .github/workflows
run typos

echo
echo "quality-fast passed"
