# Release Benchmarks

## 2026-09-13 cache-miss E2E run

This run used the repository harness
`scripts/bench-release-cache-miss-e2e.sh` on Tina, an AMD Ryzen 9 5950X with
32 logical CPUs. It was run in the existing `/home/mjc/projects/nntp-proxy`
checkout after a clean profiling build, with
`RUSTFLAGS="-C target-cpu=native"`.

The topology was:

```text
nntpbench client -> measured nntp-proxy -> nntpbench mock NNTP server
```

The harness defaults were preserved: 10 GiB transferred per scenario, 728,320
byte synthetic articles, article-only command mix, pipeline depth 32, and the
full matrix of 4 proxy thread counts (`1 2 4 8`), 7 backend connection counts
(`1 2 4 8 16 32 64`), and 4 client counts (`1 4 8 16`). One repeat was run,
for 112 completed cells.

| Summary | Value |
| --- | ---: |
| Completed cells | 112 |
| Overall mean | 3,518.8 MiB/s |
| Minimum (`threads/connections/clients`) | 305.9 MiB/s (`2/1/4`) |
| Maximum (`threads/connections/clients`) | 9,795.2 MiB/s (`2/4/4`) |
| Baseline shape (`1/1/1`) | 3,476.6 MiB/s |

The complete CSV is retained on Tina at
`target/bench-results/release-cache-miss-e2e-20260913T190920Z.csv`. It is not
checked into the repository. These numbers are evidence for this host and
configuration, not a cross-machine guarantee or a before/after claim.

## Scanner benchmark audit and RAM methodology

The earlier scanner tables are withdrawn as evidence for RAM performance.
The 8 MiB static input was repeatedly scanned without eviction and could fit
in the host's L3 cache. Its body was almost entirely repeated `x` bytes,
with no dot-stuffing. Checking only a response count did not establish that
each scanner found the correct boundaries. The reports also mislabeled
Callgrind's estimated cycles as CPU cycles, and reported Divan's 2,000 total
iterations as iterations per sample. A profile locating a search loop did
not establish the cause of the timing difference.

The corrected benchmark uses one 8 MiB stream with eight 1 MiB multiline
responses, including status lines, article headers, variable-length printable
body lines and dot-stuffing. Bytes are reproducible from a fixed random seed.
The complete wire response, including its headers and terminator, is 1 MiB;
the body alone is slightly smaller. This is a synthetic wire fixture, not a
recorded article corpus or an NNTP socket/read-buffer benchmark.

Three explicit functions perform the same traversal, starting at the current
response and searching the entire remaining stream for only its next
terminator. They differ only in search API: `find_iter(...).next()`, a Finder
constructed before measurement, or `ashwa::search_n`. They return the count
and a sum of boundary offsets. There is no enum dispatch, status-line scan,
allocation, copying, lazy initialization, per-byte accounting, or output
array in these loops. All multiline wire knowledge remains in the framer
module, as required by the repository rules.

Divan uses 300 samples of **one scan each**. Its untimed input generator
flushes every cache line of the existing buffer using CLFLUSH, then completes
the flush with MFENCE before the timer starts. It includes the final cache
line even when the allocation is unaligned. It does not refill or copy the
buffer. This creates a cold-input RAM scan; normal hardware prefetching
during the scan is allowed. Instructions and Finder metadata remain warm.
Do not override `sample_size = 1`: Divan prepares all inputs for a sample
before timing the batch, so a larger batch would reuse cached data.

Gungraun uses the same three loops. Fixture construction, boundary checks,
Finder construction and dispatch warmup run in setup, outside collection.
It reports Callgrind guest instruction counts, not measured RAM latency or
hardware cycle counts. Cache flushing is intentionally absent from this
instruction-only run.

The new fixture check compares every next-boundary result with eight known
offsets and an independent scalar scan. It also checks the scanner loops
with 64 shifted input alignments. A second check exercises empty and
unaligned cache-flush inputs and verifies that flushing preserves bytes.

The earlier branch also changed the production scanner to a global Finder.
That change has been removed. Production still eagerly collects all candidate
terminators in `MultilineFramer::split_chunk`; these three kernel measurements
do not model that policy, suffix checks, or split reads. The recorded
nntpbench E2E run above remains separate evidence and does not establish a
before/after improvement for these scanner candidates.

Finish builds before timing. Pin to one physical core, record its SMT sibling
and system I/O activity, and repeat with candidate order reversed. Preserve
timing summaries and report between-run spread. Background activity is part
of the recorded conditions; the RAM comparison below was explicitly run
under Tina's normal background load.

The audited native bench build and all three Divan cases passed in
`--test` mode. Gungraun completed all three cases:

| Scanner | Callgrind guest instructions |
| --- | ---: |
| memchr convenience | 3,686,505 |
| reusable Finder | 3,683,823 |
| Ashwa | 4,458,478 |

The complete outputs remain in
`target/bench-results/scanner-audit-gungraun.txt` and
`target/bench-results/scanner-audit-divan-test.txt` on Tina. The latter is
execution verification, not a timing measurement.

## 2026-09-13 RAM comparison after the audit

Measured code: `652a82b`, Rust 1.98.1, bench profile with native CPU flags.
Six passes used CPU 4, alternating ascending/descending candidate name order,
with 300 samples and one complete 8 MiB scan per sample. Each candidate thus
has 1,800 timed scans. Input flushing and fencing happened before every scan,
outside timing. No build or other benchmark ran concurrently.

Before measurement, CPU 4 was 99.67% idle and its sibling CPU 20 was 100%
idle over three seconds. System CPU idle was 87–90%, with ongoing background
I/O (roughly 40–55 MiB/s reads in the preflight). The user requested running
under this existing load. These observations are preflight snapshots, not a
claim that the host was isolated throughout measurement.

| Pass | Order | memchr convenience | Finder | Ashwa |
| --- | --- | ---: | ---: | ---: |
| 1 | ascending | 343.4 µs | 354.8 µs | 361.5 µs |
| 2 | descending | 366.5 µs | 354.4 µs | 383.3 µs |
| 3 | ascending | 344.6 µs | 355.7 µs | 380.0 µs |
| 4 | descending | 351.2 µs | 352.2 µs | 393.3 µs |
| 5 | ascending | 347.1 µs | 343.5 µs | 371.0 µs |
| 6 | descending | 355.2 µs | 354.5 µs | 376.3 µs |

Each cell above is that pass's median, not its minimum or mean.

| Scanner | Median of six pass medians | Range of pass medians | Throughput at summary median |
| --- | ---: | ---: | ---: |
| memchr convenience | 349.15 µs | 343.4–366.5 µs | 22.38 GiB/s |
| reusable Finder | 354.45 µs | 343.5–355.7 µs | 22.04 GiB/s |
| Ashwa | 378.15 µs | 361.5–393.3 µs | 20.66 GiB/s |

Ashwa's pass median was slower than both memchr variants in all six passes.
Its summary median is 8.3% slower than convenience and 6.7% slower than
Finder. This is substantially smaller than the withdrawn 37.9% warm-input
claim. The memchr variants each won three passes; their 1.5% aggregate
difference does not establish a reliable winner under this noise.
Individual outliers reached 1.195 ms, so tail noise is material.

Full Divan output summaries are retained at
`target/bench-results/scanner-ram-652a82b-run-{1..6}.txt` on Tina. The direct
binary invocation was run inside `nix develop`:

```bash
taskset -c 4 target/release/deps/multiline_terminator_divan-c7f49d033d36a746 --bench --sort name --sample-count 300 --sample-size 1 --threads 1
```

Even-numbered passes used `--sortr name` instead of `--sort name`.

See [../development.md](../development.md) for commands.
