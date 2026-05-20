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
