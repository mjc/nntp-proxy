#!/usr/bin/env bash
set -e

VERSION=${1:-$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')}
echo "Building release binaries for version $VERSION"
echo "Using Rust toolchain: $(rustc --version)"
echo ""

mkdir -p release/v$VERSION

# Use cargo-zigbuild which should use the nightly toolchain from PATH
if command -v cargo-zigbuild &> /dev/null; then
    BUILD_CMD="cargo-zigbuild"
else
    BUILD_CMD="cargo"
fi

echo "Build command: $BUILD_CMD"
echo ""

# Detect host platform
HOST_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
HOST_ARCH=$(uname -m)

echo "Host platform: $HOST_OS-$HOST_ARCH"
echo ""

# Linux x86_64
echo "Building for x86_64-unknown-linux-gnu..."
$BUILD_CMD build --release --target x86_64-unknown-linux-gnu
tar -czf release/v$VERSION/nntp-proxy-v$VERSION-x86_64-linux.tar.gz \
  -C target/x86_64-unknown-linux-gnu/release nntp-proxy nntp-cache-proxy

# Linux aarch64
echo "Building for aarch64-unknown-linux-gnu..."
$BUILD_CMD build --release --target aarch64-unknown-linux-gnu
tar -czf release/v$VERSION/nntp-proxy-v$VERSION-aarch64-linux.tar.gz \
  -C target/aarch64-unknown-linux-gnu/release nntp-proxy nntp-cache-proxy

# Windows x86_64
echo "Building for x86_64-pc-windows-gnu..."
$BUILD_CMD build --release --target x86_64-pc-windows-gnu
cd target/x86_64-pc-windows-gnu/release
zip ../../../release/v$VERSION/nntp-proxy-v$VERSION-x86_64-windows.zip \
  nntp-proxy.exe nntp-cache-proxy.exe
cd ../../..

# Note: macOS cross-compilation from Linux requires macOS SDK which we don't have
# Note: Windows aarch64 is not well supported by cargo-zigbuild yet
echo ""
echo "⚠️  Skipping macOS targets (require macOS SDK for cross-compilation)"
echo "⚠️  Skipping Windows aarch64 (limited toolchain support)"
echo ""
echo "To build macOS binaries, run this script on a macOS machine."
echo "To build Windows aarch64, use GitHub Actions or a Windows ARM machine."

echo ""
echo "Release binaries created in release/v$VERSION/:"
ls -lh release/v$VERSION/
