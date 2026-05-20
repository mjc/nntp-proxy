# Durable Project Memory

## NNTP Multiline Framing Rule

Never implement or modify NNTP multiline response boundary logic outside `src/session/multiline_framing.rs`.

Agents must not touch `\r\n.\r\n`, `.\r\n`, `\r`, `\n`, chunk ends, string ends, suffixes, leftovers, terminator offsets, packed response ranges, or buffer splitting after `MultilineFramer` has seen the bytes. If a response is incomplete, feed the next backend buffer back into the same `MultilineFramer`. If complete, use framer-owned typed operations.

Forbidden outside the framer: `ends_with`, `starts_with`, `windows`, manual CR/LF/dot byte checks, caller-side offset math, caller-side suffix handling, and caller-side packed-response splitting.

If a change appears to need those operations, redesign it so `MultilineFramer` owns the operation.

## Proxy Hot Path Ownership Rule

Normal proxy forwarding borrows packet data from pooled read buffers and writes
those borrowed slices directly. Do not own, clone, freeze, detach, or copy full
ARTICLE/BODY/HEAD pass-through responses into `ChunkedResponse`, `Bytes`, `Vec`,
or capture buffers. Full-response ownership is only for explicit retention:
payload caching, cache hits, list-style response exceptions, precheck/tests, or
visible pool-exhaustion fallbacks. Prefer zero-copy or borrow-only framer-owned
operations and return pooled buffers immediately after their borrowed bytes are
written.
