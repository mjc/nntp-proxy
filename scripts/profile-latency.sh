#!/usr/bin/env bash
set -e

# Latency-focused profiling for nntp-proxy
#
# Shows WHERE TIME IS SPENT WAITING — syscall latency, off-CPU time, etc.
# The runtime binary is always nntp-proxy; pass any dashboard/headless UI flags
# as extra arguments when profiling a specific UI mode.
#
# Outputs:
#   strace mode:  strace.log + summary
#   offcpu mode:  flamegraph-offcpu.svg

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

MODE="${1:-strace}"
shift 1 2>/dev/null || true

TARGET="proxy"
if [ $# -gt 0 ]; then
    case "$1" in
        proxy|ui|headless|cli|tui|/*|./*|../*)
            TARGET="$1"
            shift
            ;;
    esac
fi

EXTRA_ARGS=("$@")

if [ "$MODE" = "-h" ] || [ "$MODE" = "--help" ]; then
    echo "Usage: $0 [MODE] [TARGET] [ARGS...]"
    echo ""
    echo "Profile nntp-proxy latency and waiting patterns"
    echo ""
    echo "Modes:"
    echo "  strace  - Record syscall latency (default)"
    echo "  offcpu  - Off-CPU flamegraph (what we're waiting on)"
    echo ""
    echo "Arguments:"
    echo "  TARGET    proxy/ui/headless (all map to nntp-proxy) or a custom path"
    echo "  ARGS...   Extra arguments passed to the binary"
    echo ""
    echo "Examples:"
    echo "  ./scripts/profile-latency.sh strace proxy --config config.toml"
    echo "  ./scripts/profile-latency.sh strace ui --config config.toml [ui flags]"
    echo "  ./scripts/profile-latency.sh offcpu headless --config config.toml"
    echo ""
    echo "Stop the proxy normally to generate reports."
    exit 0
fi

# Resolve binary name
case "$TARGET" in
    proxy|ui|headless|cli|tui)
        BINARY="$PROJECT_DIR/target/profiling/nntp-proxy"
        BIN_NAME="nntp-proxy"
        ;;
    *)
        BINARY="$TARGET"
        BIN_NAME=""  # Custom path, skip build
        ;;
esac

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null
sudo chmod -R a+rx /sys/kernel/tracing 2>/dev/null || true
sudo chmod -R a+rx /sys/kernel/debug/tracing 2>/dev/null || true

# Build with profiling flags (only the binary we need)
if [ -n "$BIN_NAME" ]; then
    echo "Building $BIN_NAME..."
    RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --profile profiling --features zlib-ng --bin "$BIN_NAME"
fi

echo "=== Latency Profile Mode: $MODE ==="
echo ""

case "$MODE" in
  strace)
    echo "Recording syscall latency with strace..."
    echo "Stop the proxy to generate report."
    echo ""

    # -T: show time spent in syscall
    # -f: follow forks
    # -tt: microsecond timestamps
    # -e: trace I/O and network syscalls
    strace -T -f -tt \
      -e read,write,recvfrom,sendto,poll,epoll_wait,epoll_ctl,pselect6,open,openat,close,pread64,pwrite64,io_uring_enter \
      -o strace.log \
      "$BINARY" "${EXTRA_ARGS[@]}" || true

    echo ""
    echo "=== Syscall Summary ==="
    echo ""

    echo "Top syscalls by total time:"
    grep -oP '<[0-9.]+>' strace.log 2>/dev/null | tr -d '<>' | \
      awk '{sum+=$1; count++} END {if(count>0) printf "Total: %.3fs across %d calls (avg %.3fms)\n", sum, count, (sum/count)*1000}' || echo "(no data)"

    echo ""
    echo "Breakdown by syscall type:"
    for syscall in read write recvfrom sendto poll epoll_wait epoll_ctl pselect6 open openat close pread64 pwrite64 io_uring_enter; do
      if grep -q "^[0-9].*$syscall(" strace.log 2>/dev/null; then
        grep "$syscall(" strace.log 2>/dev/null | grep -oP '<[0-9.]+>' | tr -d '<>' | \
          awk -v name="$syscall" '{sum+=$1; count++} END {if(count>0) printf "  %-18s: %.3fs total, %6d calls, avg %.3fms\n", name, sum, count, (sum/count)*1000}'
      fi
    done

    echo ""
    echo "Slowest individual syscalls (>1ms):"
    grep -oP '^[0-9]+\s+[0-9:.]+\s+\S+\(.*<[0-9.]+>' strace.log 2>/dev/null | \
      awk -F'<' '{time=$2; gsub(/>.*/, "", time); if(time+0 > 0.001) print time, $1}' | \
      sort -rn | head -20 || echo "(no data)"

    echo ""
    echo "Full logs: strace.log"
    ;;

  offcpu)
    echo "Recording off-CPU time (what we're waiting on)..."
    echo "Stop the proxy to generate flamegraph."
    echo ""

    OFFCPU_METHOD=""

    if perf record -e sched:sched_switch -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf sched:sched_switch..."
      OFFCPU_METHOD="perf-sched"
    elif perf record -e cpu-clock -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf cpu-clock (less accurate, shows on-CPU not off-CPU)..."
      OFFCPU_METHOD="perf-cpu"
    else
      echo "Error: No off-CPU profiling method available"
      echo ""
      echo "Try fixing perf permissions:"
      echo "  sudo sh -c 'echo 0 > /proc/sys/kernel/perf_event_paranoid'"
      echo "  sudo chmod -R a+rx /sys/kernel/tracing"
      exit 1
    fi

    set +e
    case "$OFFCPU_METHOD" in
      perf-sched)
        "$BINARY" "${EXTRA_ARGS[@]}" &
        APP_PID=$!
        sleep 0.5
        perf sched record -p $APP_PID -o perf-offcpu.data
        wait $APP_PID || true
        ;;

      perf-cpu)
        "$BINARY" "${EXTRA_ARGS[@]}" &
        APP_PID=$!
        sleep 0.5
        perf record -p $APP_PID -e cpu-clock -g --call-graph fp -F 997 -o perf-offcpu.data
        wait $APP_PID || true
        ;;
    esac
    set -e

    echo ""
    echo "Generating reports..."

    if [ -f perf-offcpu.data ]; then
      if [ "$OFFCPU_METHOD" = "perf-sched" ]; then
        echo ""
        echo "=== Scheduler Latency Summary ==="
        echo ""
        echo "Top threads by scheduling latency (wait time before running):"
        perf sched timehist -i perf-offcpu.data 2>&1 | \
          awk 'NR>2 {print $5 " " $4}' | grep -v '^$' | sort -rn | head -30 || true
        echo ""
        echo "For detailed analysis:"
        echo "  perf sched timehist -i perf-offcpu.data | less"
      else
        if command -v inferno-collapse-perf &> /dev/null; then
          perf script -i perf-offcpu.data 2>/dev/null | \
            inferno-collapse-perf | \
            inferno-flamegraph --title "Off-CPU Time" > flamegraph-offcpu.svg
          echo "Done: flamegraph-offcpu.svg"
        else
          echo "inferno not found, skipping flamegraph generation"
          echo "Install with: cargo install inferno"
        fi
      fi
    else
      echo "Warning: Could not generate perf data"
    fi

    echo ""
    echo "Output files: perf-offcpu.data (and flamegraph-offcpu.svg if generated)"
    ;;

  *)
    echo "Usage: $0 [strace|offcpu] [BIN] [ARGS...]"
    echo ""
    echo "Modes:"
    echo "  strace  - Record syscall latency (default)"
    echo "  offcpu  - Off-CPU flamegraph"
    exit 1
    ;;
esac
