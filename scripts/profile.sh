#!/usr/bin/env bash
set -e

# CPU profiling for nntp-proxy
#
# Builds with frame pointers, runs under perf, generates flamegraph.
# Press 'q' in the TUI to stop recording and generate the flamegraph.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

ATTACH_PID=""
BIN="tui"
EXTRA_ARGS=()

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            echo "Usage: $0 [--pid PID] [BIN] [ARGS...]"
            echo ""
            echo "Profile nntp-proxy CPU usage"
            echo ""
            echo "Options:"
            echo "  --pid PID   Attach to an already-running process instead of launching one"
            echo ""
            echo "Arguments:"
            echo "  BIN       Which binary to profile: tui, cli, or a path (default: tui)"
            echo "  ARGS...   Extra arguments passed to the binary"
            echo ""
            echo "Examples:"
            echo "  ./scripts/profile.sh tui"
            echo "  ./scripts/profile.sh tui -c config.toml"
            echo "  ./scripts/profile.sh cli -c config.toml"
            echo "  ./scripts/profile.sh --pid 12345"
            echo ""
            echo "Press 'q' in the TUI (or Ctrl-C for CLI) to stop and generate flamegraph.svg"
            exit 0
            ;;
        --pid)
            ATTACH_PID="$2"
            shift 2
            ;;
        *)
            if [ -z "${BIN_SET:-}" ]; then
                BIN="$1"
                BIN_SET=1
                shift
            else
                EXTRA_ARGS+=("$1")
                shift
            fi
            ;;
    esac
done

# Resolve binary name
case "$BIN" in
    tui) BINARY="$PROJECT_DIR/target/profiling/nntp-proxy-tui" ;;
    cli) BINARY="$PROJECT_DIR/target/profiling/nntp-proxy" ;;
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

# Set terminal title for tmux/terminal identification
printf '\033]0;perf: nntp-proxy CPU\007'

# Build with native CPU + frame pointers
if [ -z "$ATTACH_PID" ]; then
    RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --profile profiling
fi

# Record using frame pointers
set +e
if [ -n "$ATTACH_PID" ]; then
    echo "Attaching to PID $ATTACH_PID..."
    echo "Press Ctrl-C to stop recording and generate flamegraph."
    echo ""
    perf record -g --call-graph fp -F 997 -p "$ATTACH_PID"
else
    echo "Profiling: $BINARY ${EXTRA_ARGS[*]}"
    echo "Stop the proxy (q in TUI, Ctrl-C for CLI) to generate flamegraph."
    echo ""
    perf record -g --call-graph fp -F 997 "$BINARY" "${EXTRA_ARGS[@]}"
fi
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
