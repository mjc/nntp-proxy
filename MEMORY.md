# Durable Project Memory

## NNTP Multiline Framing Rule

Never implement or modify NNTP multiline response boundary logic outside `src/session/multiline_framing.rs`.

Agents must not touch `\r\n.\r\n`, `.\r\n`, `\r`, `\n`, chunk ends, string ends, suffixes, leftovers, terminator offsets, packed response ranges, or buffer splitting after `MultilineFramer` has seen the bytes. If a response is incomplete, feed the next backend buffer back into the same `MultilineFramer`. If complete, use framer-owned typed operations.

Forbidden outside the framer: `ends_with`, `starts_with`, `windows`, manual CR/LF/dot byte checks, caller-side offset math, caller-side suffix handling, and caller-side packed-response splitting.

If a change appears to need those operations, redesign it so `MultilineFramer` owns the operation.
