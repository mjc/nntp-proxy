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

