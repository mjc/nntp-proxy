#!/usr/bin/env bash
set -e

# CPU profiling for nntp-proxy
#
# Builds with frame pointers, runs under perf, generates flamegraph.
# Press 'q' in the TUI to stop recording and generate the flamegraph.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

BIN="${1:-tui}"
shift 2>/dev/null || true
EXTRA_ARGS=("$@")

if [ "$BIN" = "-h" ] || [ "$BIN" = "--help" ]; then
    echo "Usage: $0 [BIN] [ARGS...]"
    echo ""
    echo "Profile nntp-proxy CPU usage"
    echo ""
    echo "Arguments:"
    echo "  BIN       Which binary to profile: tui, cli, or a path (default: tui)"
    echo "  ARGS...   Extra arguments passed to the binary"
    echo ""
    echo "Examples:"
    echo "  ./scripts/profile.sh tui"
    echo "  ./scripts/profile.sh tui -c config.toml"
    echo "  ./scripts/profile.sh cli -c config.toml"
    echo ""
    echo "Press 'q' in the TUI (or Ctrl-C for CLI) to stop and generate flamegraph.svg"
    exit 0
fi

# Resolve binary name
case "$BIN" in
    tui) BINARY="$PROJECT_DIR/target/release/nntp-proxy-tui" ;;
    cli) BINARY="$PROJECT_DIR/target/release/nntp-proxy" ;;
    *)   BINARY="$BIN" ;;
esac

# Check deps
if ! command -v inferno-collapse-perf &> /dev/null; then
    echo "Installing inferno..."
    cargo install inferno
fi

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null

# Build with native CPU + frame pointers
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --release

echo "Profiling: $BINARY ${EXTRA_ARGS[*]}"
echo "Stop the proxy (q in TUI, Ctrl-C for CLI) to generate flamegraph."
echo ""

# Record using frame pointers
set +e
perf record -g --call-graph fp -F 997 "$BINARY" "${EXTRA_ARGS[@]}"
set -e

echo ""
echo "Generating flamegraph from perf.data..."

if [ ! -f perf.data ]; then
    echo "Error: perf.data not found"
    exit 1
fi

perf script 2>/dev/null | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg

echo "Done: flamegraph.svg"
echo ""
echo "Open with: firefox flamegraph.svg"
echo "Analyze with: ./scripts/parse_flamegraph flamegraph.svg summary"
