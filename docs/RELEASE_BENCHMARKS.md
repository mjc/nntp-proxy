# Release Benchmarks

These are current cache-miss end-to-end release checks using:

```text
nntpbench client -> nntp-proxy -> nntpbench server
```

The 10GiB default matrices are complete single-run sweeps across 112 settings:
`threads=1 2 4 8`, `backend_connections=1 2 4 8 16 32 64`, and
`clients=1 4 8 16`, with client threads fixed at 4 and pipeline depth fixed at
32.

## 100GiB Spot Checks

### Ryzen 9 5950X

| Shape | Runs | Mean MiB/s | Median MiB/s | Min | Max | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Direct upstream `-/0/8` | 1 | 11307.07 | 11307.07 | 11307.07 | 11307.07 | No proxy, 8 client connections, pipeline depth 32 |
| Proxy `1/1/1` | 10 | 4712.73 | 4690.46 | 4465.91 | 4930.83 | 1 proxy thread, 1 client connection, 1 backend connection, pipeline depth 32 |
| Proxy `4/8/8` | 10 | 10019.26 | 10054.19 | 9471.35 | 10438.91 | 4 proxy threads, 8 client connections, 8 backend connections, pipeline depth 32 |

### Apple M1

Apple M1 with 16 GiB RAM, running on AC power. These Darwin runs used
OS-default socket buffers because the 16 MiB nntpbench socket buffers used on
the Ryzen host hit `ENOBUFS` on macOS.

| Shape | Runs | Mean MiB/s | Median MiB/s | Min | Max | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Direct upstream `-/0/8` | 1 | 7557.08 | 7557.08 | 7557.08 | 7557.08 | No proxy, 8 client connections, pipeline depth 32 |
| Proxy `1/1/1` | 10 | 3208.73 | 3197.19 | 2992.52 | 3492.82 | 1 proxy thread, 1 client connection, 1 backend connection, pipeline depth 32 |
| Proxy `4/8/8` | 10 | 3856.64 | 3850.47 | 3728.93 | 4073.03 | 4 proxy threads, 8 client connections, 8 backend connections, pipeline depth 32 |

## 10GiB Default Matrix

### Summary

| Platform | Socket buffers | Best overall | Best one-proxy-thread row | Smallest fast shape |
| --- | --- | ---: | ---: | ---: |
| Ryzen 9 5950X | 16 MiB | `8/8/4` at 11004.94 MiB/s | `1/2/16` at 6235.59 MiB/s | `1/1/1` at 4797.65 MiB/s |
| Apple M1, 16 GiB RAM | OS-default Darwin | `2/2/4` at 6004.22 MiB/s | `1/2/16` at 5404.49 MiB/s | `1/1/1` at 3699.32 MiB/s |

### Ryzen 9 5950X

| Situation | Proxy threads | Client connections | Per-backend max connections | Pipeline depth | MiB/s | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Smallest fast shape | 1 | 1 | 1 | 32 | 4797.65 | Scale backend count to paid providers |
| One client connection, tuned | 1 | 1 | 4 | 32 | 5117.08 | Best 1-proxy-thread row for one client connection |
| Several client connections, one proxy thread | 1 | 4 | 64 | 32 | 6015.25 | Best 1-proxy-thread row for four client connections |
| Many client connections, one proxy thread | 1 | 16 | 2 | 32 | 6235.59 | Best 1-proxy-thread row for sixteen client connections |
| Single-run ceiling | 8 | 4 | 8 | 32 | 11004.94 | Highest row in this sweep; not the default recommendation |

Best observed row by proxy thread count:

| Proxy threads | Best shape | Best MiB/s | Mean across rows |
| ---: | --- | ---: | ---: |
| 1 | `1/2/16` | 6235.59 | 4954.63 |
| 2 | `2/16/8` | 10688.49 | 7832.05 |
| 4 | `4/4/4` | 10535.46 | 7744.88 |
| 8 | `8/8/4` | 11004.94 | 7939.23 |

Backend connection sweep for `pipeline_depth=32` rows (MiB/s, higher is better;
cells are with one proxy thread):

| Client connections | Backend 1 | Backend 2 | Backend 4 | Backend 8 | Backend 16 | Backend 32 | Backend 64 | Best backend |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 4798 | 4450 | 5117 | 4758 | 4830 | 4665 | 4891 | 4 |
| 4 | 4538 | 5849 | 5379 | 5322 | 5128 | 4867 | 6015 | 64 |
| 8 | 4593 | 5706 | 4979 | 5320 | 4578 | 4614 | 5386 | 2 |
| 16 | 5185 | 6236 | 4105 | 6036 | 3664 | 3481 | 4240 | 2 |

### Apple M1

Apple M1 with 16 GiB RAM, running on AC power with OS-default Darwin socket
buffers.

| Situation | Proxy threads | Client connections | Per-backend max connections | Pipeline depth | MiB/s | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Smallest fast shape | 1 | 1 | 1 | 32 | 3699.32 | Scale backend count to paid providers |
| One client connection, tuned | 1 | 1 | 4 | 32 | 3738.13 | Best 1-proxy-thread row for one client connection |
| Several client connections, one proxy thread | 1 | 4 | 2 | 32 | 5352.71 | Best 1-proxy-thread row for four client connections |
| Many client connections, one proxy thread | 1 | 16 | 2 | 32 | 5404.49 | Best 1-proxy-thread row for sixteen client connections |
| Single-run ceiling | 2 | 4 | 2 | 32 | 6004.22 | Highest row in this sweep; not the default recommendation |

Best observed row by proxy thread count:

| Proxy threads | Best shape | Best MiB/s | Mean across rows |
| ---: | --- | ---: | ---: |
| 1 | `1/2/16` | 5404.49 | 4102.71 |
| 2 | `2/2/4` | 6004.22 | 4748.13 |
| 4 | `4/8/16` | 5678.01 | 4849.43 |
| 8 | `8/8/16` | 5832.80 | 4885.81 |

Backend connection sweep for `pipeline_depth=32` rows (MiB/s, higher is better;
cells are with one proxy thread):

| Client connections | Backend 1 | Backend 2 | Backend 4 | Backend 8 | Backend 16 | Backend 32 | Backend 64 | Best backend |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 3699 | 3706 | 3738 | 3713 | 3730 | 3714 | 3734 | 4 |
| 4 | 3729 | 5353 | 5052 | 4970 | 4908 | 4907 | 4880 | 2 |
| 8 | 3721 | 5293 | 5015 | 3644 | 3509 | 3433 | 3392 | 2 |
| 16 | 3742 | 5404 | 5096 | 3547 | 3113 | 3064 | 3070 | 2 |

Instrumented `perf` and heaptrack runs should be used for attribution, not
throughput claims; they materially change scheduling and absolute speed.
