# Release Benchmarks

## 2026-09-13 main cache-miss E2E baseline

This is the existing `main` baseline at `145b36c6b86621dccfe965926b0ef501ffd33b06`,
not a reused-Finder run. It used the repository harness
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

## 2026-09-13 reused-Finder E2E comparison

The candidate reuses one lazily initialized `memchr::memmem::Finder` in the
production framer. Candidate collection, suffix checks, split-read handling,
and all caller behavior are unchanged. Measured source is benchmark commit
`72ec27c3717ed463eb4c6f349c92754804123766` plus this scanner substitution;
the measured `src/session/multiline_framing.rs` Git blob is
`d6c57a5f1902daa0bc5f8f0404dc928a0751e508`.

After a whole-target `cargo clean` (preserving existing results), the E2E
script built the proxy and supplied nntpbench itself. It completed all 112
default cells with the same native flags, profiling profile, 10 GiB target,
article size, pipeline depth, and matrix as main. No affinity override or
`SKIP_BUILDS` was used. The pinned nntpbench revision was
`f4d0c98ca26ffb7bc75377e69a04ef73fd0891db`. Tina's existing background load
was allowed; no separate build or benchmark ran during measurement.

```bash
RUSTFLAGS="-C target-cpu=native" \
RESULT_FILE=target/bench-results/release-cache-miss-e2e-memchr-finder.csv \
nix develop -c scripts/bench-release-cache-miss-e2e.sh
```

Rows were paired by threads, backend connections, clients, and repeat. Each
CSV has 112 unique matching cells, each reaching at least 10 GiB. Request
counts were 14,741–14,742; byte-target overshoot varies slightly, so throughput
uses actual response bytes. Means below give each cell equal weight.

| Proxy threads | Main mean MiB/s | Finder mean MiB/s | Change | Main proxy CPU s | Finder proxy CPU s |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 3,186.4 | 3,236.0 | +1.6% | 57.34 | 56.85 |
| 2 | 3,705.4 | 4,086.7 | +10.3% | 68.69 | 64.57 |
| 4 | 3,562.8 | 3,918.5 | +10.0% | 74.40 | 71.43 |
| 8 | 3,620.6 | 3,621.6 | +0.03% | 72.88 | 73.54 |
| All 112 cells | 3,518.8 | 3,715.7 | +5.6% | 273.31 | 266.39 |

Total measured proxy CPU fell 2.5%. The median paired throughput change was
only +0.6%, and the geometric mean paired change was +3.2%. Finder won 65
cells and lost 47; 40 improved by more than 5%, while 23 regressed by more
than 5%. Individual changes ranged from -46.5% (`2/4/4`) to +67.1% (`2/16/8`).
The `1/1/1` case rose from 3,476.6 to 3,772.1 MiB/s (+8.5%), with proxy CPU
falling from 2.24 to 2.08 seconds. Total bytes divided by total measured
elapsed time rose from 1,363.8 to 1,376.4 MiB/s (+0.9%); this is different
from the equal-weight arithmetic mean of cell rates above.

This is one full matrix per variant, not a repeated interleaved experiment.
The overall direction is encouraging, but the spread and near-flat paired
median do not establish a reliable speedup or a regression-free change.
The Finder substitution remains an experiment on the benchmark branch.

Candidate CSV and script log remain on Tina at
`target/bench-results/release-cache-miss-e2e-memchr-finder.csv` and
`target/bench-results/release-cache-miss-e2e-memchr-finder.log`.

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

The global production Finder was removed during the scanner audit and then
reintroduced for the E2E comparison above. Production still eagerly collects
all candidate terminators in `MultilineFramer::split_chunk`; these three
kernel measurements do not model that policy, suffix checks, or split reads.
The stock nntpbench E2E comparison evaluates the actual production path.

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
