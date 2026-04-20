#!/bin/sh
# Install git hooks for nntp-proxy development
# Run this script after cloning the repository: ./scripts/install-git-hooks.sh

set -e

HOOKS_DIR=".git/hooks"
HOOK_FILE="$HOOKS_DIR/pre-commit"

if [ ! -d "$HOOKS_DIR" ]; then
    echo "Error: .git/hooks directory not found. Are you in the repository root?"
    exit 1
fi

echo "Installing pre-commit hook..."

cat > "$HOOK_FILE" << 'EOF'
#!/bin/sh
# Pre-commit hook for nntp-proxy
# Runs cargo fmt and cargo clippy before allowing commits, falling back to Nix
# when the required cargo tooling is not available locally.

set -e

retry_in_nix() {
    if command -v nix >/dev/null 2>&1 && [ -f "flake.nix" ]; then
        echo "$1"
        shift
        exec nix develop -c env NNTP_PROXY_PRE_COMMIT_IN_NIX=1 "$0" "$@"
    fi
}

run_checks() {
    echo "Running cargo fmt..."
    if ! cargo fmt --check; then
        echo "❌ Code is not formatted. Run 'cargo fmt' to fix formatting."
        return 1
    fi
    echo "✅ Code is properly formatted"

    echo ""
    echo "Running cargo clippy..."
    if ! cargo clippy --all-targets --all-features -- -D warnings; then
        echo "❌ Clippy found issues. Fix them before committing."
        return 1
    fi
    echo "✅ No clippy warnings"

    echo ""
    echo "✅ All pre-commit checks passed!"
}

missing_tools=0

if ! command -v cargo >/dev/null 2>&1; then
    missing_tools=1
elif ! cargo fmt --version >/dev/null 2>&1; then
    missing_tools=1
elif ! cargo clippy --version >/dev/null 2>&1; then
    missing_tools=1
fi

if [ -z "${NNTP_PROXY_PRE_COMMIT_IN_NIX:-}" ]; then
    if [ "$missing_tools" -ne 0 ]; then
        retry_in_nix "Cargo tooling not found; entering Nix development environment..." "$@"
        echo "Error: cargo, rustfmt, or clippy is not available, and nix develop cannot be used."
        exit 1
    fi
fi

if ! run_checks; then
    if [ -z "${NNTP_PROXY_PRE_COMMIT_IN_NIX:-}" ]; then
        retry_in_nix "Local pre-commit checks failed; retrying inside Nix development environment..." "$@"
    fi
    exit 1
fi
EOF

chmod +x "$HOOK_FILE"

echo "✅ Pre-commit hook installed successfully!"
echo ""
echo "The hook will run the following checks before each commit:"
echo "  - cargo fmt --check (code formatting)"
echo "  - cargo clippy --all-targets --all-features -- -D warnings (linting)"
echo "If local cargo tooling is missing or cannot run the checks, the hook will retry inside nix develop."
