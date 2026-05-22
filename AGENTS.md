# Agent Rules For NNTP Response Framing

This file is mandatory guidance for AI agents working in this repository.

## Do Not Touch Multiline Boundaries Outside The Framer

`src/session/multiline_framing.rs` is the only place allowed to know how NNTP multiline responses end.

Do not inspect, compare, split, optimize, or reason about any of these outside that module:

- `\r\n.\r\n`
- `.\r\n`
- `\r`
- `\n`
- chunk ends
- string ends
- suffixes
- leftovers
- terminator offsets
- packed response byte ranges

After code calls `MultilineFramer`, the response is either handled by framer-owned operations or the same `MultilineFramer` receives the next backend buffer. Do not add caller-side split state, offset math, `ends_with`, `starts_with`, `windows`, manual CR/LF checks, or “fast paths”.

## Borrow Packet Data On Proxy Hot Paths

This is a proxy. The normal forwarding path must borrow packet/response bytes
from the current pooled read buffer and write those borrowed slices directly.
Do not own, clone, freeze, detach, copy into `ChunkedResponse`, `Bytes`, `Vec`, or
capture buffers on the pass-through ARTICLE/BODY/HEAD path.

Owning a full response is only acceptable for explicit retention paths:

- article/body/head payload caching
- cache hits serving already-owned payloads
- list-style/XOVER/OVER/HDR paths that intentionally need an owned response
- tests or precheck paths that explicitly require ownership
- pool exhaustion fallbacks, which must remain visible in metrics

If a change wants to keep packet data after the current write, treat that as a
design smell. Prefer zero-copy or borrow-only handoff from framer-owned typed
operations. Release or reuse pooled buffers as soon as their borrowed bytes have
been written.

Forbidden outside `src/session/multiline_framing.rs`:

```rust
data.ends_with(b"\r\n.\r\n")
data.ends_with(b".\r\n")
line == b".\r\n"
line.starts_with(b".")
chunk.windows(...).any(...)
let response = &buf[..end];
let suffix = &buf[end..];
```

If a change seems to need any of that, the design is wrong. Move the behavior into `MultilineFramer` and expose a higher-level operation such as write, capture, cache ingest, or ordered response emission.

## Preserve Retry Tier Semantics

Do not route ARTICLE/BODY/HEAD retry attempts to a lower-priority tier just
because the preferred tier is saturated, has waiters, or looks slower in a
profile. Tier escalation is only allowed after every eligible backend in the
current tier has actually returned the article-missing response for that
request and has been recorded missing in `ArticleAvailability`.

Throughput work on the retry path must preserve that invariant. Optimize
connection reuse, buffering, lock lifetime, request pipelining, and selection
inside the current eligible tier; do not use pool pressure as a reason to skip
ahead to tier 1+ before tier 0 has all 430s.

## Perf Data Parsing

When inspecting `perf.data`, prefer the repo helper:

```sh
nix develop -c scripts/parse_perfdata perf.data
```

It reads `perf.data` directly and emits the thread, instruction-pointer, edge,
timeline, and category summaries agents usually need for profile triage. Do not
replace it with a text-rendered parser; the point of this helper is to avoid
waiting on perf's renderer for large callchains.
