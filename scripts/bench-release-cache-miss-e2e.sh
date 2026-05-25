#!/usr/bin/env bash
set -euo pipefail

# End-to-end profiling-profile benchmark for large article cache-miss performance.
#
# Topology:
#   nntpbench client -> measured nntp-proxy -> nntpbench mock NNTP server
#
# The client and backend both come from nntpbench. The harness always feeds the
# client a synthetic segments file by default so ARTICLE/BODY requests stay on
# comparable message-ID paths across nntpbench revisions. The measured proxy
# has no [cache] section: every measured request passes through the front proxy
# to nntpbench.

RESULT_DIR=${RESULT_DIR:-"target/bench-results"}
WORK_DIR=${WORK_DIR:-"$RESULT_DIR/release-cache-miss-e2e-work"}
TIMESTAMP=$(date -u +"%Y%m%dT%H%M%SZ")
RESULT_FILE=${RESULT_FILE:-"$RESULT_DIR/release-cache-miss-e2e-$TIMESTAMP.csv"}

TARGET_ARTICLE_BYTES=${TARGET_ARTICLE_BYTES:-${DATASET_BYTES:-10737418240}}
TRANSFER_REPEAT_FACTOR=${TRANSFER_REPEAT_FACTOR:-${SEGMENT_REPEAT_FACTOR:-1}}
TRANSFER_BYTES_PER_SCENARIO=${TRANSFER_BYTES_PER_SCENARIO:-0}
REQUESTS_PER_SCENARIO=${REQUESTS_PER_SCENARIO:-0}

NNTPBENCH_FLAKE_REF=${NNTPBENCH_FLAKE_REF:-"git+https://github.com/mjc/nntpbench.git?rev=f4d0c98ca26ffb7bc75377e69a04ef73fd0891db"}
NNTPBENCH_OUT_LINK=${NNTPBENCH_OUT_LINK:-"$WORK_DIR/nix-nntpbench"}
NNTPBENCH_BIN=${NNTPBENCH_BIN:-"$NNTPBENCH_OUT_LINK/bin/nntpbench"}
NNTPBENCH_ARTICLE_BYTES=${NNTPBENCH_ARTICLE_BYTES:-728320}
NNTPBENCH_BODY_BYTES=${NNTPBENCH_BODY_BYTES:-$NNTPBENCH_ARTICLE_BYTES}
NNTPBENCH_MAX_CONNECTIONS=${NNTPBENCH_MAX_CONNECTIONS:-4096}
NNTPBENCH_MAX_PIPELINE_DEPTH=${NNTPBENCH_MAX_PIPELINE_DEPTH:-1024}
NNTPBENCH_START_ID=${NNTPBENCH_START_ID:-1}
NNTPBENCH_SEGMENTS_MODE=${NNTPBENCH_SEGMENTS_MODE:-always}
NNTPBENCH_SEGMENT_COUNT=${NNTPBENCH_SEGMENT_COUNT:-65536}
NNTPBENCH_SEGMENTS_FILE=${NNTPBENCH_SEGMENTS_FILE:-"$WORK_DIR/nntpbench-synthetic-segments.tsv"}

SKIP_BUILDS=${SKIP_BUILDS:-0}
THREADS_MATRIX=${THREADS_MATRIX:-"1 2 4 8"}
CONNECTIONS_MATRIX=${CONNECTIONS_MATRIX:-"1 2 4 8 16 32 64"}
CLIENTS_MATRIX=${CLIENTS_MATRIX:-"1 4 8 16"}
CLIENT_PROCESSES=${CLIENT_PROCESSES:-1}
CLIENT_THREADS=${CLIENT_THREADS:-4}
CLIENT_PIPELINE_DEPTH=${CLIENT_PIPELINE_DEPTH:-32}
CLIENT_COMMAND_MIX=${CLIENT_COMMAND_MIX:-article}
REPEAT_COUNT=${REPEAT_COUNT:-1}
PROFILE_UPSTREAM=${PROFILE_UPSTREAM:-0}
PROFILE_MEASURED=${PROFILE_MEASURED:-0}
PROFILE_MEASURED_HEAP=${PROFILE_MEASURED_HEAP:-0}
PROFILE_EVENT=${PROFILE_EVENT:-cpu-clock}
PROFILE_FREQ=${PROFILE_FREQ:-997}
TOKIO_CONSOLE=${TOKIO_CONSOLE:-0}
TOKIO_CONSOLE_PROCESS=${TOKIO_CONSOLE_PROCESS:-measured}
SHARDED_MEASURED_INSTANCES=${SHARDED_MEASURED_INSTANCES:-1}
DIRECT_UPSTREAM=${DIRECT_UPSTREAM:-0}
PROXY_CLI_MODE=${PROXY_CLI_MODE:-current}
HOST=${HOST:-127.0.0.1}
UPSTREAM_TASKSET=${UPSTREAM_TASKSET:-""}
MEASURED_TASKSET=${MEASURED_TASKSET:-""}
CLIENT_TASKSET=${CLIENT_TASKSET:-""}

CARGO_PROFILE=${CARGO_PROFILE:-profiling}

if [ "$TARGET_ARTICLE_BYTES" -le 0 ]; then
    echo "TARGET_ARTICLE_BYTES must be greater than zero" >&2
    exit 1
fi

if [ "$TRANSFER_REPEAT_FACTOR" -le 0 ]; then
    echo "TRANSFER_REPEAT_FACTOR must be greater than zero" >&2
    exit 1
fi

mkdir -p "$RESULT_DIR" "$WORK_DIR"

UPSTREAM_LOG="$WORK_DIR/upstream.log"
MEASURED_LOG="$WORK_DIR/measured.log"

MEASURED_PIDS=()
MEASURED_WRAPPER_PIDS=()
MEASURED_PORTS=()
UPSTREAM_PID=""
MEASURED_PID=""

should_use_nntpbench_segments() {
    case "$NNTPBENCH_SEGMENTS_MODE" in
        always|on|true|1)
            return 0
            ;;
        never|off|false|0)
            return 1
            ;;
        auto)
            return 0
            ;;
        *)
            echo "NNTPBENCH_SEGMENTS_MODE must be one of: auto, always, never" >&2
            exit 1
            ;;
    esac
}

ensure_nntpbench_segments_file() {
    if ! should_use_nntpbench_segments; then
        return
    fi

    echo "Using synthetic nntpbench segments file at $NNTPBENCH_SEGMENTS_FILE"

    python3 - "$NNTPBENCH_SEGMENTS_FILE" "$NNTPBENCH_SEGMENT_COUNT" "$NNTPBENCH_ARTICLE_BYTES" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
count = int(sys.argv[2])
size = int(sys.argv[3])

path.parent.mkdir(parents=True, exist_ok=True)
with path.open("w", encoding="utf-8") as fh:
    for idx in range(count):
        fh.write(f"{size}\tbench.segment.{idx}@nntpbench.local\n")
PY
}

reserve_port() {
    python3 - <<'PY'
import socket

s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_for_port() {
    python3 - "$1" "$2" "$3" <<'PY'
import socket
import sys
import time

host = sys.argv[1]
port = int(sys.argv[2])
timeout = float(sys.argv[3])
deadline = time.monotonic() + timeout

while time.monotonic() < deadline:
    try:
        with socket.create_connection((host, port), timeout=0.25):
            raise SystemExit(0)
    except OSError:
        time.sleep(0.05)

raise SystemExit(f"timeout waiting for {host}:{port}")
PY
}

sum_proc_ticks() {
    python3 - "$@" <<'PY'
import pathlib
import sys

total = 0
for pid in sys.argv[1:]:
    task_dir = pathlib.Path("/proc") / pid / "task"
    try:
        stat_paths = list(task_dir.glob("*/stat"))
    except OSError:
        stat_paths = []

    if not stat_paths:
        stat_paths = [pathlib.Path("/proc") / pid / "stat"]

    for stat_path in stat_paths:
        try:
            stat = stat_path.read_text(encoding="utf-8")
        except OSError:
            continue
        rest = stat[stat.rfind(") ") + 2 :].split()
        total += int(rest[11]) + int(rest[12])

print(total)
PY
}

proc_tids_csv() {
    python3 - "$1" <<'PY'
import pathlib
import sys

task_dir = pathlib.Path("/proc") / sys.argv[1] / "task"
try:
    tids = sorted(path.name for path in task_dir.iterdir() if path.name.isdigit())
except OSError:
    tids = [sys.argv[1]]

print(",".join(tids))
PY
}

sum_proc_rss() {
    python3 - "$@" <<'PY'
import sys

total = 0
for pid in sys.argv[1:]:
    try:
        for line in open(f"/proc/{pid}/status", "r", encoding="utf-8"):
            if line.startswith("VmRSS:"):
                total += int(line.split()[1])
                break
    except OSError:
        pass

print(total)
PY
}

calc_mib_per_sec() {
    python3 - "$1" "$2" <<'PY'
import sys

print(float(sys.argv[1]) / 1024 / 1024 / float(sys.argv[2]))
PY
}

calc_articles_per_sec() {
    python3 - "$1" "$2" <<'PY'
import sys

print(float(sys.argv[1]) / float(sys.argv[2]))
PY
}

finish_perf_record() {
    local pid="$1"
    local data="$2"
    local report="$3"
    local label="$4"

    if [ -z "$pid" ]; then
        return
    fi

    kill -INT "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    perf report --stdio --no-children -i "$data" >"$report" 2>&1 || true
    echo "$label perf data: $data"
    echo "$label perf report: $report"
}

run_with_optional_taskset() {
    local cpu_list="$1"
    shift

    if [ -n "$cpu_list" ]; then
        taskset -c "$cpu_list" "$@"
    else
        "$@"
    fi
}

run_with_optional_taskset_exec() {
    local cpu_list="$1"
    shift

    if [ -n "$cpu_list" ]; then
        exec taskset -c "$cpu_list" "$@"
    else
        exec "$@"
    fi
}

resolve_transfer_bytes_per_scenario() {
    if [ "$TRANSFER_BYTES_PER_SCENARIO" -gt 0 ]; then
        echo "$TRANSFER_BYTES_PER_SCENARIO"
    else
        echo $((TARGET_ARTICLE_BYTES * TRANSFER_REPEAT_FACTOR))
    fi
}

compute_shard_quota() {
    local total="$1"
    local total_clients="$2"
    local offset="$3"
    local shard_clients="$4"

    if [ "$total" -le 0 ] || [ "$total_clients" -le 0 ] || [ "$shard_clients" -le 0 ]; then
        echo 0
        return
    fi

    local per_client=$((total / total_clients))
    local extra_clients=$((total % total_clients))
    local quota=$((shard_clients * per_client))

    if [ "$offset" -lt "$extra_clients" ]; then
        local shard_end=$((offset + shard_clients))
        local extra_for_shard="$shard_clients"
        if [ "$shard_end" -gt "$extra_clients" ]; then
            extra_for_shard=$((extra_clients - offset))
        fi
        quota=$((quota + extra_for_shard))
    fi

    echo "$quota"
}

build_proxy_binary() {
    local build_rustflags="${RUSTFLAGS:-}"
    local -a build_features=()

    if [ "$TOKIO_CONSOLE" = "1" ]; then
        build_features=(--features tokio-console)
        if [[ " $build_rustflags " != *" --cfg tokio_unstable "* ]]; then
            build_rustflags="${build_rustflags:+$build_rustflags }--cfg tokio_unstable"
        fi
    fi

    echo "Building $CARGO_PROFILE binary"
    if [ "$SKIP_BUILDS" = "0" ]; then
        if [ "$CARGO_PROFILE" = "release" ]; then
            RUSTFLAGS="$build_rustflags" cargo build --release --bin nntp-proxy "${build_features[@]}"
        else
            RUSTFLAGS="$build_rustflags" cargo build --profile "$CARGO_PROFILE" --bin nntp-proxy "${build_features[@]}"
        fi
    fi

    BIN="./target/$CARGO_PROFILE/nntp-proxy"
}

build_nntpbench_binary() {
    echo "Building pinned nntpbench package from flake ref $NNTPBENCH_FLAKE_REF"
    if [ "$SKIP_BUILDS" = "0" ] || [ ! -x "$NNTPBENCH_BIN" ]; then
        nix build --out-link "$NNTPBENCH_OUT_LINK" "$NNTPBENCH_FLAKE_REF"
    fi
}

start_upstream() {
    UPSTREAM_PORT=$(reserve_port)

    echo "Using flake-provided nntpbench backend at $NNTPBENCH_BIN"
    run_with_optional_taskset_exec "$UPSTREAM_TASKSET" "$NNTPBENCH_BIN" server \
        --listen "$HOST:$UPSTREAM_PORT" \
        --body-bytes "$NNTPBENCH_BODY_BYTES" \
        --article-bytes "$NNTPBENCH_ARTICLE_BYTES" \
        --max-connections "$NNTPBENCH_MAX_CONNECTIONS" \
        --max-pipeline-depth "$NNTPBENCH_MAX_PIPELINE_DEPTH" \
        --stats-interval-secs 0 \
        >"$UPSTREAM_LOG" 2>&1 &
    UPSTREAM_PID=$!

    wait_for_port "$HOST" "$UPSTREAM_PORT" 60
    echo "Using nntpbench backend on $HOST:$UPSTREAM_PORT"
    echo "nntpbench_article_bytes=$NNTPBENCH_ARTICLE_BYTES nntpbench_max_connections=$NNTPBENCH_MAX_CONNECTIONS nntpbench_max_pipeline_depth=$NNTPBENCH_MAX_PIPELINE_DEPTH"
}

stop_measured_processes() {
    if [ "${#MEASURED_PIDS[@]}" -gt 0 ]; then
        kill "${MEASURED_PIDS[@]}" 2>/dev/null || true
        if [ "${#MEASURED_WRAPPER_PIDS[@]}" -eq 0 ]; then
            wait "${MEASURED_PIDS[@]}" 2>/dev/null || true
        fi
    fi
    if [ "${#MEASURED_WRAPPER_PIDS[@]}" -gt 0 ]; then
        wait "${MEASURED_WRAPPER_PIDS[@]}" 2>/dev/null || true
    fi
    MEASURED_PIDS=()
    MEASURED_WRAPPER_PIDS=()
    MEASURED_PORTS=()
    MEASURED_PID=""
}

cleanup() {
    set +e
    stop_measured_processes
    if [ -n "$UPSTREAM_PID" ]; then
        kill "$UPSTREAM_PID" 2>/dev/null || true
        wait "$UPSTREAM_PID" 2>/dev/null || true
    fi
}

trap cleanup EXIT

write_measured_config() {
    local threads="$1"
    local backend_connections="$2"
    local clients="$3"
    local repeat_index="$4"
    local shard_index="$5"
    local shard_port="$6"
    local config_path="$7"

    cat > "$config_path" <<EOF
[proxy]
host = "$HOST"
port = $shard_port
threads = $threads
log_file_level = "warn"
stats_file = "$WORK_DIR/measured-$threads-$backend_connections-$clients-repeat-$repeat_index-shard-$shard_index-stats.json"

[routing]
mode = "per-command"
backend_selection = "least-loaded"
adaptive_precheck = false

[memory]
socket_recv_buffer_size = 16777216
socket_send_buffer_size = 16777216
buffer_pool_size = 1048576
buffer_pool_count = 128
capture_pool_size = 1048576
capture_pool_count = 64

[[servers]]
host = "$HOST"
port = $UPSTREAM_PORT
name = "nntpbench-backend"
max_connections = $backend_connections
tier = 0
use_tls = false
EOF
}

start_measured_shards() {
    local threads="$1"
    local backend_connections="$2"
    local clients="$3"
    local repeat_index="$4"

    MEASURED_PIDS=()
    MEASURED_WRAPPER_PIDS=()
    MEASURED_PORTS=()

    if [ "$SHARDED_MEASURED_INSTANCES" -gt 1 ]; then
        if [ "$PROFILE_MEASURED" = "1" ]; then
            echo "PROFILE_MEASURED is not supported with SHARDED_MEASURED_INSTANCES > 1" >&2
            exit 1
        fi
        if [ "$PROFILE_MEASURED_HEAP" = "1" ]; then
            echo "PROFILE_MEASURED_HEAP is not supported with SHARDED_MEASURED_INSTANCES > 1" >&2
            exit 1
        fi

        local _
        for _ in $(seq 1 "$SHARDED_MEASURED_INSTANCES"); do
            MEASURED_PORTS+=("$(reserve_port)")
        done
    else
        if [ -z "${MEASURED_PORT:-}" ]; then
            MEASURED_PORT=$(reserve_port)
        fi
        MEASURED_PORTS=("$MEASURED_PORT")
    fi

    local shard_index
    for shard_index in "${!MEASURED_PORTS[@]}"; do
        local shard_port="${MEASURED_PORTS[$shard_index]}"
        local shard_log="$WORK_DIR/measured-$threads-$backend_connections-$clients-repeat-$repeat_index-shard-$shard_index.log"
        local config_path="$WORK_DIR/measured-$threads-$backend_connections-$clients-repeat-$repeat_index-shard-$shard_index.toml"
        local -a measured_env=()
        local -a proxy_cmd

        if [ "$SHARDED_MEASURED_INSTANCES" -eq 1 ]; then
            shard_log="$MEASURED_LOG"
        fi

        write_measured_config \
            "$threads" \
            "$backend_connections" \
            "$clients" \
            "$repeat_index" \
            "$shard_index" \
            "$shard_port" \
            "$config_path"

        if [ "$TOKIO_CONSOLE" = "1" ] && [ "$TOKIO_CONSOLE_PROCESS" = "measured" ]; then
            measured_env+=(NNTP_PROXY_TOKIO_CONSOLE=1)
        fi
        proxy_cmd=("$BIN" --config "$config_path")
        if [ "$PROXY_CLI_MODE" != "legacy" ]; then
            proxy_cmd+=(--ui headless)
        fi
        if [ "$PROFILE_MEASURED_HEAP" = "1" ]; then
            proxy_cmd=(heaptrack --record-only -o "$WORK_DIR/measured-heap-$threads-$backend_connections-$clients-repeat-$repeat_index-shard-$shard_index" "${proxy_cmd[@]}")
        fi

        if [ "${#measured_env[@]}" -gt 0 ]; then
            (
                export "${measured_env[@]}"
                run_with_optional_taskset_exec "$MEASURED_TASKSET" "${proxy_cmd[@]}"
            ) >"$shard_log" 2>&1 &
        else
            run_with_optional_taskset_exec "$MEASURED_TASKSET" "${proxy_cmd[@]}" >"$shard_log" 2>&1 &
        fi
        local launched_pid="$!"
        if [ "$PROFILE_MEASURED_HEAP" = "1" ]; then
            MEASURED_WRAPPER_PIDS+=("$launched_pid")
        else
            MEASURED_PIDS+=("$launched_pid")
        fi
    done

    local shard_port
    for shard_port in "${MEASURED_PORTS[@]}"; do
        wait_for_port "$HOST" "$shard_port" 60
    done

    if [ "$PROFILE_MEASURED_HEAP" = "1" ]; then
        local wrapper_pid
        for wrapper_pid in "${MEASURED_WRAPPER_PIDS[@]}"; do
            local child_pid=""
            for _ in $(seq 1 50); do
                child_pid=$(pgrep -P "$wrapper_pid" -f 'nntp-proxy' | head -1 || true)
                if [ -n "$child_pid" ]; then
                    break
                fi
                sleep 0.1
            done
            if [ -z "$child_pid" ]; then
                echo "Could not find heaptrack child process for wrapper $wrapper_pid" >&2
                exit 1
            fi
            MEASURED_PIDS+=("$child_pid")
        done
    fi
    MEASURED_PID="${MEASURED_PIDS[0]}"
}

run_client_load() {
    local port="$1"
    local ports="$2"
    local clients="$3"
    local requests="$4"
    local transfer_bytes="$5"
    local client_offset="${6:-0}"
    local total_clients="${7:-$clients}"
    local run_dir="${8:-$WORK_DIR/client-load-$$-$RANDOM}"
    local process_count="$CLIENT_PROCESSES"

    if [ "$process_count" -le 1 ] || [ "$clients" -le 1 ]; then
        local -a cmd=("$NNTPBENCH_BIN" client --connect "$HOST:$port")
        if [ -n "$ports" ]; then
            cmd+=(--ports "$ports")
        fi
        if should_use_nntpbench_segments; then
            cmd+=(--segments "$NNTPBENCH_SEGMENTS_FILE")
        fi
        cmd+=(
            --requests "$requests"
            --transfer-bytes "$transfer_bytes"
            --start-id "$((NNTPBENCH_START_ID + client_offset))"
            --connections "$clients"
            --threads "$CLIENT_THREADS"
            --pipeline-depth "$CLIENT_PIPELINE_DEPTH"
            --command-mix "$CLIENT_COMMAND_MIX"
            --socket-recv-buffer 16777216
            --socket-send-buffer 16777216
            --stats-interval-secs 0
            --csv
        )
        if [ "$total_clients" -gt 0 ]; then
            cmd+=(--total-clients "$total_clients")
        fi
        if [ "$client_offset" -gt 0 ]; then
            cmd+=(--client-offset "$client_offset")
        fi
        run_with_optional_taskset "$CLIENT_TASKSET" "${cmd[@]}"
        return
    fi

    if [ "$process_count" -gt "$clients" ]; then
        process_count="$clients"
    fi

    mkdir -p "$run_dir"

    local start
    start=$(python3 - <<'PY'
import time
print(time.perf_counter())
PY
)

    local -a pids=()
    local offset=0
    local process_idx
    for process_idx in $(seq 0 $((process_count - 1))); do
        local process_clients=$((clients / process_count))
        if [ "$process_idx" -lt $((clients % process_count)) ]; then
            process_clients=$((process_clients + 1))
        fi

        local process_requests
        local process_transfer_bytes
        process_requests=$(compute_shard_quota "$requests" "$clients" "$offset" "$process_clients")
        process_transfer_bytes=$(compute_shard_quota "$transfer_bytes" "$clients" "$offset" "$process_clients")

        local -a cmd=("$NNTPBENCH_BIN" client --connect "$HOST:$port")
        if [ -n "$ports" ]; then
            cmd+=(--ports "$ports")
        fi
        if should_use_nntpbench_segments; then
            cmd+=(--segments "$NNTPBENCH_SEGMENTS_FILE")
        fi
        cmd+=(
            --requests "$process_requests"
            --transfer-bytes "$process_transfer_bytes"
            --start-id "$((NNTPBENCH_START_ID + offset))"
            --connections "$process_clients"
            --threads "$CLIENT_THREADS"
            --pipeline-depth "$CLIENT_PIPELINE_DEPTH"
            --client-offset "$offset"
            --total-clients "$clients"
            --command-mix "$CLIENT_COMMAND_MIX"
            --socket-recv-buffer 16777216
            --socket-send-buffer 16777216
            --stats-interval-secs 0
            --csv
        )
        run_with_optional_taskset_exec "$CLIENT_TASKSET" "${cmd[@]}" >"$run_dir/client-$process_idx.csv" &
        pids+=("$!")
        offset=$((offset + process_clients))
    done

    local status=0
    local pid
    for pid in "${pids[@]}"; do
        if ! wait "$pid"; then
            status=1
        fi
    done
    if [ "$status" -ne 0 ]; then
        return "$status"
    fi

    python3 - "$run_dir" "$start" <<'PY'
import glob
import os
import sys
import time

run_dir = sys.argv[1]
start = float(sys.argv[2])
requests = 0
bytes_ = 0
cpu = 0.0
rss = 0

for path in glob.glob(os.path.join(run_dir, "client-*.csv")):
    line = open(path, "r", encoding="utf-8").read().strip()
    if not line:
        continue
    req, size, _elapsed, proc_cpu, proc_rss = line.split(",")
    requests += int(req)
    bytes_ += int(size)
    cpu += float(proc_cpu)
    rss += int(proc_rss)

elapsed = time.perf_counter() - start
print(f"{requests},{bytes_},{elapsed:.9f},{cpu:.9f},{rss}", flush=True)
PY
}

run_direct_upstream_matrix() {
    local requests="$1"
    local transfer_bytes="$2"

    local clients
    for clients in $CLIENTS_MATRIX; do
        local perf_upstream_pid=""
        local perf_upstream_data="$WORK_DIR/upstream-direct-perf-$clients.data"
        local perf_upstream_report="$WORK_DIR/upstream-direct-perf-$clients.txt"

        if [ "$PROFILE_UPSTREAM" = "1" ]; then
            rm -f "$perf_upstream_data" "$perf_upstream_report"
            perf record -e "$PROFILE_EVENT" -F "$PROFILE_FREQ" -g -t "$(proc_tids_csv "$UPSTREAM_PID")" -o "$perf_upstream_data" -- sleep 3600 >"$WORK_DIR/upstream-direct-perf.log" 2>&1 &
            perf_upstream_pid=$!
            sleep 0.2
        fi

        local result
        local result_status=0
        result=$(run_client_load "$UPSTREAM_PORT" "" "$clients" "$requests" "$transfer_bytes" 0 "$clients") || result_status=$?

        finish_perf_record "$perf_upstream_pid" "$perf_upstream_data" "$perf_upstream_report" "Upstream direct"
        if [ "$result_status" -ne 0 ]; then
            return "$result_status"
        fi

        IFS=, read -r reqs bytes elapsed client_cpu client_rss <<<"$result"
        local mib_s
        local articles_s
        mib_s=$(calc_mib_per_sec "$bytes" "$elapsed")
        articles_s=$(calc_articles_per_sec "$reqs" "$elapsed")
        echo "direct,0,$clients,1,$reqs,$bytes,$elapsed,$client_cpu,$client_rss,0,0,$mib_s,$articles_s" | tee -a "$RESULT_FILE"
    done

    echo "Results: $RESULT_FILE"
    echo "Logs: $WORK_DIR"
}

run_measured_scenario() {
    local threads="$1"
    local backend_connections="$2"
    local clients="$3"
    local repeat_index="$4"
    local requests="$5"
    local transfer_bytes="$6"

    start_measured_shards "$threads" "$backend_connections" "$clients" "$repeat_index"

    local before_ticks
    before_ticks=$(sum_proc_ticks "${MEASURED_PIDS[@]}")

    local perf_upstream_pid=""
    local perf_upstream_data="$WORK_DIR/upstream-perf-$threads-$backend_connections-$clients-repeat-$repeat_index.data"
    local perf_upstream_report="$WORK_DIR/upstream-perf-$threads-$backend_connections-$clients-repeat-$repeat_index.txt"
    if [ "$PROFILE_UPSTREAM" = "1" ]; then
        rm -f "$perf_upstream_data" "$perf_upstream_report"
        perf record -e "$PROFILE_EVENT" -F "$PROFILE_FREQ" -g -t "$(proc_tids_csv "$UPSTREAM_PID")" -o "$perf_upstream_data" -- sleep 3600 >"$WORK_DIR/upstream-perf.log" 2>&1 &
        perf_upstream_pid=$!
        sleep 0.2
    fi

    local perf_measured_pid=""
    local perf_measured_data="$WORK_DIR/measured-perf-$threads-$backend_connections-$clients-repeat-$repeat_index.data"
    local perf_measured_report="$WORK_DIR/measured-perf-$threads-$backend_connections-$clients-repeat-$repeat_index.txt"
    if [ "$PROFILE_MEASURED" = "1" ]; then
        rm -f "$perf_measured_data" "$perf_measured_report"
        perf record -e "$PROFILE_EVENT" -F "$PROFILE_FREQ" -g -t "$(proc_tids_csv "$MEASURED_PID")" -o "$perf_measured_data" -- sleep 3600 >"$WORK_DIR/measured-perf.log" 2>&1 &
        perf_measured_pid=$!
        sleep 0.2
    fi

    local result
    local result_status=0
    result=$(run_client_load \
        "${MEASURED_PORTS[0]}" \
        "$(IFS=,; echo "${MEASURED_PORTS[*]}")" \
        "$clients" \
        "$requests" \
        "$transfer_bytes" \
        0 \
        "$clients" \
        "$WORK_DIR/client-load-$threads-$backend_connections-$clients-repeat-$repeat_index") || result_status=$?

    finish_perf_record "$perf_upstream_pid" "$perf_upstream_data" "$perf_upstream_report" "Upstream"
    finish_perf_record "$perf_measured_pid" "$perf_measured_data" "$perf_measured_report" "Measured"
    if [ "$result_status" -ne 0 ]; then
        stop_measured_processes
        return "$result_status"
    fi

    local after_ticks
    after_ticks=$(sum_proc_ticks "${MEASURED_PIDS[@]}")
    local proxy_cpu
    proxy_cpu=$(python3 - "$before_ticks" "$after_ticks" <<'PY'
import sys

print((int(sys.argv[2]) - int(sys.argv[1])) / 100.0)
PY
)
    local proxy_rss
    proxy_rss=$(sum_proc_rss "${MEASURED_PIDS[@]}")

    IFS=, read -r reqs bytes elapsed client_cpu client_rss <<<"$result"
    local mib_s
    local articles_s
    mib_s=$(calc_mib_per_sec "$bytes" "$elapsed")
    articles_s=$(calc_articles_per_sec "$reqs" "$elapsed")
    echo "$threads,$backend_connections,$clients,$repeat_index,$reqs,$bytes,$elapsed,$client_cpu,$client_rss,$proxy_cpu,$proxy_rss,$mib_s,$articles_s" | tee -a "$RESULT_FILE"

    stop_measured_processes
}

UPSTREAM_PORT=""
MEASURED_PORT="$(reserve_port)"
SCENARIO_TRANSFER_BYTES="$(resolve_transfer_bytes_per_scenario)"

echo "Scenario target: requests=${REQUESTS_PER_SCENARIO:-0} transfer_bytes=$SCENARIO_TRANSFER_BYTES repeat_factor=$TRANSFER_REPEAT_FACTOR"

build_proxy_binary
build_nntpbench_binary
ensure_nntpbench_segments_file
start_upstream

echo "Writing results to $RESULT_FILE"
echo "threads,backend_connections,clients,repeat,requests,response_bytes,elapsed_s,client_cpu_s,client_rss_kib,measured_proxy_cpu_s,measured_proxy_rss_kib,mib_s,articles_s" > "$RESULT_FILE"

if [ "$DIRECT_UPSTREAM" = "1" ]; then
    run_direct_upstream_matrix "$REQUESTS_PER_SCENARIO" "$SCENARIO_TRANSFER_BYTES"
    exit 0
fi

read -r -a threads_values <<<"$THREADS_MATRIX"
read -r -a backend_values <<<"$CONNECTIONS_MATRIX"
read -r -a client_values <<<"$CLIENTS_MATRIX"

for threads in "${threads_values[@]}"; do
    for backend_connections in "${backend_values[@]}"; do
        for clients in "${client_values[@]}"; do
            for repeat_index in $(seq 1 "$REPEAT_COUNT"); do
                run_measured_scenario \
                    "$threads" \
                    "$backend_connections" \
                    "$clients" \
                    "$repeat_index" \
                    "$REQUESTS_PER_SCENARIO" \
                    "$SCENARIO_TRANSFER_BYTES"
            done
        done
    done
done

echo "Results: $RESULT_FILE"
echo "Logs: $WORK_DIR"
