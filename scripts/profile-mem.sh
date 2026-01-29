#!/usr/bin/env bash
set -e

# Heap profiling for nntp-proxy
#
# Uses heaptrack (LD_PRELOAD-based, no code changes needed) to profile
# memory allocations. Falls back to valgrind --tool=massif if heaptrack
# is unavailable.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

BIN="${1:-tui}"
shift 2>/dev/null || true
EXTRA_ARGS=("$@")

if [ "$BIN" = "-h" ] || [ "$BIN" = "--help" ]; then
    echo "Usage: $0 [BIN] [ARGS...]"
    echo ""
    echo "Profile nntp-proxy heap allocations"
    echo ""
    echo "Arguments:"
    echo "  BIN       Which binary to profile: tui, cli, or a path (default: tui)"
    echo "  ARGS...   Extra arguments passed to the binary"
    echo ""
    echo "Tools (in preference order):"
    echo "  heaptrack   - Fast LD_PRELOAD-based heap profiler"
    echo "  massif      - Valgrind heap profiler (slower, more detailed)"
    echo ""
    echo "Examples:"
    echo "  ./scripts/profile-mem.sh tui"
    echo "  ./scripts/profile-mem.sh tui -c config.toml"
    echo "  ./scripts/profile-mem.sh cli -c config.toml"
    echo ""
    echo "Stop the proxy (q in TUI, Ctrl-C for CLI) to generate the report."
    exit 0
fi

# Resolve binary name
case "$BIN" in
    tui) BINARY="$PROJECT_DIR/target/profiling/nntp-proxy-tui" ;;
    cli) BINARY="$PROJECT_DIR/target/profiling/nntp-proxy" ;;
    *)   BINARY="$BIN" ;;
esac

# Build with profiling flags
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --profile profiling

echo "=== Heap Profiling ==="
echo ""

if command -v heaptrack &> /dev/null; then
    echo "Using heaptrack..."
    echo "Profiling: $BINARY ${EXTRA_ARGS[*]}"
    echo "Stop the proxy to generate report."
    echo ""

    heaptrack "$BINARY" "${EXTRA_ARGS[@]}" || true

    # Find the most recent heaptrack output
    HEAPTRACK_FILE=$(ls -t heaptrack.*.zst 2>/dev/null | head -1)

    if [ -n "$HEAPTRACK_FILE" ]; then
        echo ""
        echo "Heaptrack data: $HEAPTRACK_FILE"
        echo ""

        if command -v heaptrack_print &> /dev/null; then
            echo "Generating text summary..."
            heaptrack_print "$HEAPTRACK_FILE" -F stacks.txt > heaptrack-summary.txt 2>&1 || true
            echo "Summary: heaptrack-summary.txt"
            echo "Stacks:  stacks.txt"
        fi

        if command -v heaptrack_gui &> /dev/null; then
            echo ""
            echo "Open GUI with: heaptrack_gui $HEAPTRACK_FILE"
        fi
    else
        echo "Warning: No heaptrack output file found"
    fi

elif command -v valgrind &> /dev/null; then
    echo "heaptrack not found, falling back to valgrind massif..."
    echo "Profiling: $BINARY ${EXTRA_ARGS[*]}"
    echo "Stop the proxy to generate report."
    echo ""

    valgrind --tool=massif --pages-as-heap=yes \
        "$BINARY" "${EXTRA_ARGS[@]}" || true

    # Find the most recent massif output
    MASSIF_FILE=$(ls -t massif.out.* 2>/dev/null | head -1)

    if [ -n "$MASSIF_FILE" ]; then
        echo ""
        echo "Massif data: $MASSIF_FILE"

        if command -v ms_print &> /dev/null; then
            echo ""
            echo "=== Massif Summary ==="
            ms_print "$MASSIF_FILE" | head -50
            echo ""
            echo "Full report: ms_print $MASSIF_FILE | less"
        fi
    else
        echo "Warning: No massif output file found"
    fi

else
    echo "Error: Neither heaptrack nor valgrind found"
    echo ""
    echo "Install one of:"
    echo "  heaptrack (recommended): sudo apt install heaptrack"
    echo "  valgrind:                sudo apt install valgrind"
    exit 1
fi
