# Durable Project Memory

Keep this file short. It exists to preserve project invariants that are easy for
agents to break.

## Multiline Framing

Only `src/session/multiline_framing.rs` may inspect NNTP multiline boundaries:
CR/LF/dot bytes, chunk ends, suffixes, leftovers, terminator offsets, or packed
response ranges. Callers must feed incomplete responses back into the same
`MultilineFramer` and use framer-owned write/capture/observe/cache operations.

## Hot Path Ownership

Normal ARTICLE/BODY/HEAD pass-through writes borrowed slices from pooled backend
read buffers. Do not own, clone, freeze, detach, or copy those responses unless
the path explicitly retains data for payload caching, cache-hit rendering,
list-style response ownership, tests/prechecks, or visible pool-exhaustion or
oversized-retention fallback.
