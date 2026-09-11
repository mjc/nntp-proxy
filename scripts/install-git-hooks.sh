#!/bin/sh
# Install git hooks for nntp-proxy development
# Run this script after cloning the repository: ./install-git-hooks.sh

set -e

HOOKS_DIR="$(git rev-parse --path-format=absolute --git-path hooks)"
HOOK_FILE="$HOOKS_DIR/pre-commit"

if [ ! -d "$HOOKS_DIR" ]; then
    echo "Error: .git/hooks directory not found. Are you in the repository root?"
    exit 1
fi

echo "Installing pre-commit hook..."

cat > "$HOOK_FILE" << 'EOF'
#!/bin/sh
# Pre-commit hook for nntp-proxy
# Runs the fast nntp-proxy quality gate before allowing commits.
# If the repository has a Nix flake, prefer the dev shell for consistent tooling.

set -e

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

if [ -z "${NNTP_PROXY_PRE_COMMIT_IN_NIX:-}" ] \
    && command -v nix >/dev/null 2>&1 \
    && [ -f "flake.nix" ]; then
    echo "Entering Nix development environment for pre-commit checks..."
    exec nix develop -c env NNTP_PROXY_PRE_COMMIT_IN_NIX=1 "$0" "$@"
fi

scripts/quality-fast.sh
EOF

chmod +x "$HOOK_FILE"

echo "✅ Pre-commit hook installed successfully!"
echo ""
echo "The hook will run the following checks before each commit:"
echo "  - scripts/quality-fast.sh"
echo "  - nix develop -c ... when flake.nix and nix are available"
echo ""
echo "To bypass the hook temporarily, use: git commit --no-verify"
