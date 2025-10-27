#!/bin/sh
# Install git hooks for nntp-proxy development
# Run this script after cloning the repository: ./install-git-hooks.sh

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
# Runs cargo fmt and cargo clippy before allowing commits

set -e

# Unset cross-compilation env vars that might interfere
unset CC CXX AR

echo "Running cargo fmt..."
cargo fmt --check
if [ $? -ne 0 ]; then
    echo "❌ Code is not formatted. Run 'cargo fmt' to fix formatting."
    exit 1
fi
echo "✅ Code is properly formatted"

echo ""
echo "Running cargo clippy..."
cargo clippy --all-features -- -D warnings
if [ $? -ne 0 ]; then
    echo "❌ Clippy found issues. Fix them before committing."
    exit 1
fi
echo "✅ No clippy warnings"

echo ""
echo "✅ All pre-commit checks passed!"
EOF

chmod +x "$HOOK_FILE"

echo "✅ Pre-commit hook installed successfully!"
echo ""
echo "The hook will run the following checks before each commit:"
echo "  - cargo fmt --check (code formatting)"
echo "  - cargo clippy --all-features (linting)"
echo ""
echo "To bypass the hook temporarily, use: git commit --no-verify"
