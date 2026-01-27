#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# NNTP Proxy Release Build Script
# ============================================================================
# Builds cross-platform release binaries for Linux, Windows, and macOS.
#
# Usage:
#   ./scripts/build-release.sh [VERSION]
#
# Cross-compilation strategy:
#   - Linux (x86_64, aarch64): cargo-zigbuild with Zig linker
#   - Windows (x86_64): cargo-zigbuild with Zig linker + ring crypto
#   - macOS (universal): native cargo build with lipo (macOS host only)
#
# TLS crypto providers:
#   - Linux: aws-lc-rs (faster, uses AWS-LC via CMAKE)
#   - Windows: ring (pure Rust, avoids CMAKE conflicts)
#   - macOS: aws-lc-rs (native builds work fine)
# ============================================================================

# Check for jq if version argument is not provided
if [ -z "$1" ]; then
    if ! command -v jq &> /dev/null; then
        echo "Error: jq is required but not installed. Please install jq to continue." >&2
        exit 1
    fi
fi

# Determine version from argument or Cargo.toml
VERSION=${1:-$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')}

# Validate VERSION to prevent directory traversal attacks
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][a-zA-Z0-9._-]+)?$ ]]; then
    echo "Error: Invalid version format '$VERSION'. Expected semver format (e.g., 1.0.0)." >&2
    exit 1
fi

# Detect host platform
HOST_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
HOST_ARCH=$(uname -m)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ============================================================================
# Helper Functions
# ============================================================================

log_info() {
    echo -e "${BLUE}ℹ${NC} $1"
}

log_success() {
    echo -e "${GREEN}✅${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}⚠️${NC}  $1"
}

log_error() {
    echo -e "${RED}❌${NC} $1"
}

build_linux_target() {
    local target=$1
    local arch=$2
    
    log_info "Building for $target..."
    $BUILD_CMD build --release --target "$target"
    tar -czf "release/v$VERSION/nntp-proxy-v$VERSION-$arch-linux.tar.gz" \
        -C "target/$target/release" nntp-proxy nntp-cache-proxy
    log_success "Built $arch Linux binary"
}

build_windows_target() {
    local target=$1
    local arch=$2

    log_info "Building for $target..."
    $BUILD_CMD build --release --target "$target"
    (cd "target/$target/release" && \
     zip "../../../release/v$VERSION/nntp-proxy-v$VERSION-$arch-windows.zip" \
         nntp-proxy.exe nntp-cache-proxy.exe)
    log_success "Built $arch Windows binary"
}

build_macos_universal() {
    log_info "Building macOS universal binaries..."
    
    # Build both architectures
    log_info "Building for x86_64-apple-darwin..."
    cargo build --release --target x86_64-apple-darwin
    
    log_info "Building for aarch64-apple-darwin..."
    cargo build --release --target aarch64-apple-darwin
    
    # Create universal binaries
    log_info "Creating universal binaries with lipo..."
    mkdir -p target/universal-apple-darwin/release
    
    for binary in nntp-proxy nntp-cache-proxy; do
        lipo -create \
            "target/x86_64-apple-darwin/release/$binary" \
            "target/aarch64-apple-darwin/release/$binary" \
            -output "target/universal-apple-darwin/release/$binary"
    done
    
    tar -czf "release/v$VERSION/nntp-proxy-v$VERSION-universal-darwin.tar.gz" \
        -C target/universal-apple-darwin/release nntp-proxy nntp-cache-proxy
    
    log_success "Built macOS universal binary"
}

# ============================================================================
# Main Build Process
# ============================================================================

echo "════════════════════════════════════════════════════════════════════════"
echo "  NNTP Proxy Release Build"
echo "  Version: $VERSION"
echo "  Host: $HOST_OS-$HOST_ARCH"
echo "════════════════════════════════════════════════════════════════════════"
echo ""

# Enter cross-compilation environment if not already in it
if [ -z "${RUSTUP_TOOLCHAIN:-}" ] || [ ! -f "/tmp/.in-nix-cross-env" ]; then
    log_info "Entering Nix cross-compilation environment..."
    touch /tmp/.in-nix-cross-env
    exec nix develop .#cross -c "$0" "$@"
fi

log_info "Using Rust toolchain: $(rustc --version)"
echo ""

# Create release directory
mkdir -p "release/v$VERSION"

# Determine build command
if command -v cargo-zigbuild &> /dev/null; then
    BUILD_CMD="cargo-zigbuild"
    log_info "Using cargo-zigbuild for cross-compilation"
else
    BUILD_CMD="cargo"
    log_warn "cargo-zigbuild not found, using regular cargo (may fail for cross-compilation)"
fi
echo ""

# ============================================================================
# Build Targets
# ============================================================================

# Linux targets (always build with cargo-zigbuild)
build_linux_target "x86_64-unknown-linux-gnu" "x86_64"
build_linux_target "aarch64-unknown-linux-gnu" "aarch64"

# Windows target (cargo-zigbuild with ring crypto)
build_windows_target "x86_64-pc-windows-gnu" "x86_64"

# macOS targets (only on macOS host, using native cargo + lipo)
if [[ "$HOST_OS" == "darwin" ]]; then
    build_macos_universal
else
    echo ""
    log_warn "Skipping macOS targets (require macOS host for native compilation)"
    log_info "To build macOS binaries, run this script on a macOS machine."
fi

# ============================================================================
# Summary
# ============================================================================

echo ""
echo "════════════════════════════════════════════════════════════════════════"
log_success "Release build complete!"
echo ""
echo "  Version: $VERSION"
echo "  Output directory: release/v$VERSION/"
echo ""
echo "Built artifacts:"
find "release/v$VERSION/" -maxdepth 1 -type f -exec ls -lh {} \; | awk '{printf "  • %s (%s)\n", $NF, $5}'
echo "════════════════════════════════════════════════════════════════════════"

# Clean up the cross-env marker
rm -f /tmp/.in-nix-cross-env

