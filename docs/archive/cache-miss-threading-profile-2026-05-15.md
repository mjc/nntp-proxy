# Cache-Miss Threading Profile - 2026-05-15

This note is historical profiling context. Do not quote these numbers as
current README or release performance claims; later forwarding changes changed
the cache-miss path. The current conservative README numbers are the 2026-05-20
100GB `nntpbench` spot checks:

| Shape | Mean MiB/s | Median MiB/s | Stdev | Min | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `1/1/1` | 3881.42 | 3895.40 | 105.28 | 3690.86 | 4003.93 |
| `4/8/8` | 8622.58 | 8618.86 | 147.46 | 8326.10 | 8808.85 |

This note captured an investigation into why adding Tokio worker
threads and backend connections does not scale the large-article cache-miss
benchmark cleanly.

## Workload

Topology:

```text
Python benchmark client
  -> measured nntp-proxy with no article cache
  -> upstream nntp-proxy with 11 GiB RAM article cache and 11 GiB disk cache
  -> Python origin fixture
```

Dataset:

- Source NZB: `testfile-10GB-no-rar.nzb`
- Selected articles: 14,746
- Selected response bytes: 10,739,812,486
- Minimum article size: 716,800 bytes
- Pipelining: enabled
- Socket buffers: 16 MiB send and receive buffers
- Build flag for benchmark runs: `RUSTFLAGS="-C target-cpu=native"`

Primary shape investigated here:

```text
clients = 200
measured backend connections = 10
measured proxy threads = 2
upstream cache proxy threads = 2
```

## Instrumentation Added

Tokio/task instrumentation:

- Optional Cargo feature: `tokio-console`
- Build requirement: `RUSTFLAGS="--cfg tokio_unstable"`
- Runtime enable: `NNTP_PROXY_TOKIO_CONSOLE=1`
- Harness controls:
  - `TOKIO_CONSOLE=1`
  - `TOKIO_CONSOLE_PROCESS=measured|upstream`
- Task names added under `tokio_unstable`:
  - `client-session`
  - `backend-pipeline-worker`

Pipeline timing instrumentation:

- Runtime enable: `NNTP_PROXY_PIPELINE_TIMING=1`
- Harness controls:
  - `PIPELINE_TIMING=1`
  - `PIPELINE_TIMING_PROCESS=measured|upstream`
- Records:
  - queue wait from client enqueue to backend worker handling
  - backend worker read/buffer time
  - client task wait on pipeline oneshot
  - client write time, including writer-lock wait

The timing instrumentation is diagnostic only. It significantly changes absolute
throughput in this workload, so use the timings to understand stage proportions,
not to quote release speed.

## Commands

Measured-side perf profile:

```bash
RUSTFLAGS="-C target-cpu=native" \
PROFILE_MEASURED=1 \
PIPELINE_TIMING=1 \
PIPELINE_TIMING_PROCESS=measured \
UPSTREAM_THREADS=2 \
THREADS_MATRIX=2 \
CONNECTIONS_MATRIX=10 \
CLIENTS_MATRIX=200 \
PRELOAD_CLIENTS=1 \
REQUESTS_PER_SCENARIO=0 \
scripts/bench-release-cache-miss-e2e.sh
```

Upstream-side perf profile:

```bash
RUSTFLAGS="-C target-cpu=native" \
PROFILE_UPSTREAM=1 \
PIPELINE_TIMING=1 \
PIPELINE_TIMING_PROCESS=upstream \
UPSTREAM_THREADS=2 \
THREADS_MATRIX=2 \
CONNECTIONS_MATRIX=10 \
CLIENTS_MATRIX=200 \
PRELOAD_CLIENTS=1 \
REQUESTS_PER_SCENARIO=0 \
scripts/bench-release-cache-miss-e2e.sh
```

Timing-only measured run:

```bash
RUSTFLAGS="-C target-cpu=native" \
PIPELINE_TIMING=1 \
PIPELINE_TIMING_PROCESS=measured \
UPSTREAM_THREADS=2 \
THREADS_MATRIX=2 \
CONNECTIONS_MATRIX=10 \
CLIENTS_MATRIX=200 \
PRELOAD_CLIENTS=1 \
REQUESTS_PER_SCENARIO=0 \
scripts/bench-release-cache-miss-e2e.sh
```

Tokio console run, for interactive task state inspection:

```bash
RUSTFLAGS="-C target-cpu=native" \
CARGO_PROFILE=profiling \
TOKIO_CONSOLE=1 \
TOKIO_CONSOLE_PROCESS=measured \
UPSTREAM_THREADS=2 \
THREADS_MATRIX=2 \
CONNECTIONS_MATRIX=10 \
CLIENTS_MATRIX=200 \
PRELOAD_CLIENTS=1 \
REQUESTS_PER_SCENARIO=0 \
scripts/bench-release-cache-miss-e2e.sh

tokio-console
```

## Results

### Corrected Client Driver

The first direct-upstream cache runs were capped by the Python benchmark client.
In those runs `client_cpu_s` was approximately equal to elapsed wall time, so a
single Python process was the bottleneck at roughly 1.6-1.8 GiB/s.

The harness now supports `CLIENT_PROCESSES=N`, which preserves the same global
client/request distribution but splits the benchmark readers across multiple OS
processes. This does not change the proxy topology or the real NZB message IDs;
it only prevents one Python event loop from being the throughput ceiling.

Corrected direct-upstream command:

```bash
RUSTFLAGS="-C target-cpu=native" \
DIRECT_UPSTREAM=1 \
UPSTREAM_THREADS=2 \
CLIENTS_MATRIX="200 200 200 200 200" \
CLIENT_PROCESSES=4 \
PRELOAD_CLIENTS=1 \
REQUESTS_PER_SCENARIO=0 \
scripts/bench-release-cache-miss-e2e.sh
```

Corrected direct-upstream results:

| Run | MiB/s | Elapsed s | Client CPU s | Result file |
| --- | ---: | ---: | ---: | --- |
| 1 | 3883.00 | 2.6377 | 9.4783 | `target/bench-results/release-cache-miss-e2e-20260515T193101Z.csv` |
| 2 | 3904.25 | 2.6234 | 9.4257 | `target/bench-results/release-cache-miss-e2e-20260515T193101Z.csv` |
| 3 | 3853.96 | 2.6576 | 9.8207 | `target/bench-results/release-cache-miss-e2e-20260515T193101Z.csv` |
| 4 | 3868.61 | 2.6475 | 9.2362 | `target/bench-results/release-cache-miss-e2e-20260515T193101Z.csv` |
| 5 | 3928.71 | 2.6070 | 9.0703 | `target/bench-results/release-cache-miss-e2e-20260515T193101Z.csv` |

Summary: mean 3887.91 MiB/s, min 3853.96, max 3928.71, range 74.74 MiB/s
(1.9% of mean). The direct hybrid cache process is much faster than the earlier
single-client-driver measurements indicated, but still well below the synthetic
moka memory-cache microbenchmark.

Corrected direct-upstream profile:

```bash
RUSTFLAGS="-C target-cpu=native" \
DIRECT_UPSTREAM=1 \
PROFILE_UPSTREAM=1 \
UPSTREAM_THREADS=2 \
CLIENTS_MATRIX=200 \
CLIENT_PROCESSES=4 \
PRELOAD_CLIENTS=1 \
REQUESTS_PER_SCENARIO=0 \
scripts/bench-release-cache-miss-e2e.sh
```

Historical profile run after explicitly keeping `CachedPayload` clones shallow:

```text
result: 4069.70 MiB/s
report: target/bench-results/release-cache-miss-e2e-work/upstream-direct-perf-200.txt

12.28%  __memmove_avx_unaligned_erms
10.13%  per_command::run_per_command_loop
 8.37%  HybridArticleCache::get_by_cache_key
 7.72%  foyer_memory::cache::Cache::get_or_fetch_inner
 7.36%  AvailabilityIndex::with_capacity_and_generation_count
 5.59%  runtime client-session closure
 2.41%  __memcmp_avx2_movbe
 2.02%  process_single_command
```

The `clone<[u8]>` attribution under `HybridArticleCache::get_by_cache_key`
remained after replacing the derived `CachedPayload` clone with explicit
`Arc::clone` calls. Treat it as symbol attribution for shallow `Arc<[u8]>`
cloning unless a heap/copy profiler proves otherwise.

### Throughput

| Run | Profiled process | Timing enabled | MiB/s | Elapsed s | Measured proxy CPU s | Result file |
| --- | --- | --- | ---: | ---: | ---: | --- |
| Baseline | none | no | 1459.93 | 7.0156 | 3.43 | `target/bench-results/release-cache-miss-e2e-20260515T175316Z.csv` |
| Perf measured | measured | measured | 621.45 | 16.4813 | 8.25 | `target/bench-results/release-cache-miss-e2e-20260515T182736Z.csv` |
| Perf upstream | upstream | upstream | 683.24 | 14.9908 | 7.07 | `target/bench-results/release-cache-miss-e2e-20260515T183601Z.csv` |
| Timing only | none | measured | 607.11 | 16.8705 | 6.17 | `target/bench-results/release-cache-miss-e2e-20260515T183915Z.csv` |

The baseline is the useful throughput datapoint. The perf/timing runs are useful
only for attribution. Timing instrumentation appears to distort scheduling and
throughput heavily in this shape.

### Measured Proxy Perf

Report:

```text
target/bench-results/release-cache-miss-e2e-work/measured-perf-2-10-200.txt
```

Top samples:

```text
48.50%  memchr::memmem::searcher::searcher_kind_avx2
 6.59%  __memmove_avx_unaligned_erms
 3.59%  per_command::run_per_command_loop
 2.67%  backend_pipeline_worker
 2.48%  BufferPool::acquire
 1.87%  runtime client-session closure
 1.29%  process_single_command
 1.27%  AvailabilityIndex::with_capacity_and_generation_count
 1.10%  await_pipeline_request
 0.70%  tokio OwnedTasks::bind_inner
 0.63%  tokio multi_thread worker::run
 0.61%  tokio waker::wake_by_val
 0.59%  tokio batch_semaphore Acquire::poll
 0.58%  deadpool timeout_get
```

This run is different from the earlier non-timing measured profile, where kernel
socket copies dominated. With timing enabled, the measured proxy is dominated by
the terminator scan. That does not mean the scanner suddenly got worse; it means
the instrumentation changed the balance enough that user-space scanning became
the most visible remaining work.

The useful architectural signal is that Tokio scheduling primitives are present
but not dominant in CPU samples. The slowdown is not explained by a single large
Tokio lock or scheduler hotspot.

### Measured Pipeline Timings

Timing-only run, final emitted line:

```text
pipeline_timing completed=14336 bytes=10441199646
  avg_queue_wait_us=3845
  avg_backend_read_us=2305
  avg_client_await_us=6453
  avg_client_write_us=359
```

Interpretation:

- Client write time is small, about 0.36 ms/article.
- Backend read/buffer time is about 2.3 ms/article.
- Queue wait is about 3.8 ms/article.
- Client await is about 6.5 ms/article because it includes time waiting for the
  worker to dequeue, read, buffer, and send the oneshot.

That points at the current architecture: the backend worker must fully read and
buffer each large response before the client task can write anything. For large
articles, this creates a two-stage handoff:

```text
backend socket -> backend worker buffer/scan -> oneshot -> client task -> client socket
```

More backend connections create more concurrent workers doing large scans and
buffer ownership transfers, but they do not reduce the per-article dependency
chain. With 10 backend connections, the system appears to spend more time in
queueing/scheduling and memory bandwidth work without increasing useful
throughput.

### Upstream Cache Perf

Report:

```text
target/bench-results/release-cache-miss-e2e-work/upstream-perf-2-10-200.txt
```

Top samples:

```text
12.70%  __memmove_avx_unaligned_erms
10.56%  foyer_memory::cache::Cache::get_or_fetch_inner
 7.92%  per_command::run_per_command_loop
 4.23%  HybridArticleCache::get_by_cache_key
 3.67%  runtime client-session closure
 2.55%  tokio multi_thread worker::run
 2.40%  _rjem_malloc
 1.78%  __vdso_clock_gettime
 1.66%  HybridGet::poll
 1.63%  tokio batch_semaphore Acquire::poll
 1.57%  clock_gettime
 1.52%  _rjem_sdallocx
 1.46%  __memcmp_avx2_movbe
 1.44%  process_single_command
 1.32%  prepare_request_execution
 1.26%  tokio task raw::poll
 1.12%  tokio worker Context::run_task
 1.09%  core::str::from_utf8
 1.09%  io_util::write_all_vectored
```

Upstream pipeline timing did not emit during the measured phase because hot cache
hits bypass the upstream proxy's backend pipeline path. That is expected for this
topology: once the upstream cache is warm, it serves from cache to socket instead
of forwarding to the Python origin.

The upstream cache profile suggests a different bottleneck from the measured
proxy:

- Foyer/memory-cache lookup is visible.
- There is still memmove/copy cost.
- Allocator activity is visible.
- `write_all_vectored` is visible but not top-level dominant.

This means the upstream cache process is not just a free RAM byte server; cache
lookup, cache record cloning, allocation, and socket delivery all contribute.

## Architecture Finding

The current pipeline architecture helps request pipelining but does not stream a
large response through a single task from backend to client. Instead:

1. Client task parses commands and enqueues a cloned `RequestContext`.
2. Backend pipeline worker dequeues requests, writes commands, reads full
   responses in backend FIFO order, scans for the terminator, and stores payload
   in `ChunkedResponse`.
3. Backend worker sends the completed context through a oneshot.
4. Client task wakes, takes the payload, locks the client writer, and writes the
   buffered response.

For large 700 KiB+ articles this means each article has to cross a task boundary
after it has already been fully received and buffered. Extra runtime threads do
not make a single article complete faster, and extra backend connections increase
the number of concurrent scans/buffers/socket copies competing for memory
bandwidth.

The evidence so far says the main issue is architectural, not simply "Tokio
multithreading is bad":

- Tokio scheduler samples exist but are small compared with scanning/cache/copy.
- Client write timing is not the main delay.
- Queue wait plus backend read/buffer dominate the measured proxy timing.
- High backend fanout makes the workload worse even with many clients.
- Upstream cache hits still have nontrivial cache lookup/allocation/copy cost.

## Current Best Hypothesis

For large-article cache-miss forwarding, the best-performing architecture is
likely closer to one of these:

1. Keep low backend fanout and use deeper per-connection pipelining.
2. Stream backend responses directly to the owning client writer from the backend
   connection task, while optionally teeing/capturing for cache.
3. Avoid handing a full 700 KiB+ buffered response from a backend worker to a
   client task unless caching/retry semantics require it.
4. For cache-hot upstream responses, reduce cache lookup/clone/allocation cost,
   especially in `HybridArticleCache::get_by_cache_key` and foyer `get`.

The hardest constraint is NNTP framing: we still have to detect `\r\n.\r\n` and
preserve packed leftover bytes. But the measured timings suggest the full-buffer
handoff is now a bigger architectural limit than client socket writing.

## Next Measurements

Recommended next steps:

1. Run `tokio-console` interactively against the measured proxy with task names
   enabled. Look for:
   - `backend-pipeline-worker` tasks spending long periods busy
   - `client-session` tasks waiting on oneshot receives
   - task migration or high wake counts
   - long poll durations on pipeline workers

2. Add histograms instead of averages for pipeline timing. The current averages
   hide whether a small number of articles stall badly.

3. Run a low-fanout comparison with the same timing instrumentation:
   - `40 clients / 2 backend / 2 threads`
   - `200 clients / 2 backend / 2 threads`

4. Prototype a streaming pipeline-worker delivery path for large `ARTICLE`/`BODY`
   responses and compare against the current full-buffer handoff path.

5. Profile upstream cache-only hot-hit serving separately from the measured
   cache-miss topology, because foyer lookup/clone/allocation is now visible in
   the upstream profile.
