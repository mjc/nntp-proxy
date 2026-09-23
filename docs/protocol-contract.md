# Response contract map

This document records the protocol boundary that the two repositories must
share before their framing and article code can move into one crate. It is a
contract map, not a promise that either adapter owns the other project's
storage.

## Wire and content contract

The request context selects the response shape. A status code alone never
selects multiline framing. In particular, GROUP treats 211 as a single-line
response while LISTGROUP treats 211 as a multiline response.

Both adapters use these wire rules:

- The response initial line ends only at CRLF.
- A single-line response ends immediately after that initial CRLF.
- A multiline response ends at the first protocol dot line (CRLF dot CRLF
  after non-empty content, or dot CRLF for an empty block).
- The framed response excludes the terminator from semantic payload content but
  retains the original wire bytes, including dot-stuffing.
- Bytes after the frame are a packed suffix belonging to the next response and
  remain available to that request.
- Fragmentation never changes the result. Scanner state carries across reads;
  callers do not rescan or reconstruct a boundary.

Article validation is a second guarantee after framing. It accepts the
request-scoped article forms (ARTICLE, HEAD, BODY, and STAT), retains parsed
first-line metadata, and keeps the validated layout tied to its bytes. Optional
yEnc validation is a policy at this semantic boundary, not a framing rule.
The framed owner always retains the original wire bytes. nntpbench's owned
validated view records header-unfold and body-unstuff requirements and uses an
owned `Cow` only when one is needed. The proxy's ordinary pooled view keeps
body bytes in their wire representation; forwarding never allocates or runs a
second body-boundary scan, while captured article consumers can choose their
own materialization policy.

## Coordinates

All endpoints are exclusive:

| Coordinate | Meaning | Owner |
| --- | --- | --- |
| ChunkConsumed | Count consumed from one multiline body-scanner push | Multiline framer |
| ResponseChunkConsumed | Count consumed from one response-decoder push | Buffered receiver |
| PendingOffset | Exclusive end of the already-scanned accumulated prefix | Buffered receiver |
| WindowOffset | Exclusive origin supplied to one streaming scanner push | Streaming framer |
| FrameEnd | Exclusive end of a complete response in the accumulated logical window | Receiver/framer |
| StatusLineEnd | Exclusive end of the request-scoped initial line | Framed article state |
| ContentEnd | Exclusive end before the multiline terminator | Framed article state |

A chunk coordinate is never a frame coordinate. Translation happens once inside
the receiver/framer operation that owns both origins. Newtypes document the
coordinate kind, but they do not identify a buffer; resource identity comes
from the owning state or exclusive borrow.

The buffered adapter uses `PendingOffset + ResponseChunkConsumed -> FrameEnd`;
the streaming adapter uses `WindowOffset + ChunkConsumed -> FrameEnd`. The
names differ only where the physical adapter has a different input boundary.

## Ownership map

### nntpbench adapter

BufferedResponseReceiver owns a `PendingInput` (the pending `BytesMut` plus
its scanned-prefix cursor) and its request-scoped decoder. `PendingInput`
performs append, read, and consuming frame extraction as one buffer-bound
operation; the decoder cannot be paired with a different pending allocation.
The decoder returns one framed immutable owner while retaining a packed suffix
in the receiver. Article layout ranges are opaque `ArticleFrameRange` values,
not independently reusable `Range<usize>` coordinates. The article typestate
transition is:

Article<Framed<Bytes>> -> Article<Validated<Bytes>>

The validated state stores the immutable bytes, private layout, first-line
metadata, and transformation requirements. OwnedResponse and OwnedArticle are
the public projections of those states.

### nntp-proxy adapter

PooledBuffer owns the allocation and visible logical window; it does not know
NNTP boundaries. A RetainedAppendPermit is consumed once and returns an
AppendOutcome whose AppendedRead borrows the same buffer.

BackendResponseExchange keeps the connection, classified response, pool, and
backend identity together. MultilineFramer and its request tracker own
continuation, packed-suffix separation, and coordinate translation in
src/session/multiline_framing.rs. Ordinary ARTICLE/BODY/HEAD forwarding borrows
the pooled window; capture and cache paths may intentionally retain complete
bytes.

The proxy article transition has the same semantic meaning as nntpbench's:

Article<Framed<B>> -> Article<Validated<B>>

but B is selected by the adapter (PooledBuffer, Bytes, or another stable
owner). No proxy pass-through response is forced through the owned client
representation.

## Deliberate adapter differences

These are compatibility decisions, not alternate boundary definitions:

- nntpbench freezes a completed response because its public client returns an
  immutable owned value. The proxy forwards ordinary article responses from
  pooled borrowed bytes and only owns a complete response for capture, cache,
  or other intentional retention.
- The proxy's transparent forwarding tracker preserves an unparseable status
  line as a tracked wire response for existing connection/order behavior.
  nntpbench's public decoder rejects that malformed initial line. Article
  validation and capture remain strict in both projects. This exception must
  remain an adapter policy if the core is extracted; it is not permission to
  accept malformed CRLF in semantic article parsing.
- Connection pooling, cancellation retirement, and local ordered replies are
  proxy concerns. Pending immutable response ownership and public future
  cancellation are nntpbench concerns.

The neutral corpus in tests/fixtures/response_contract.rs is run through the
production framing boundary in each repository. It covers single-line errors
and STAT, empty and stuffed multiline bodies, folded HEAD, the
GROUP/LISTGROUP 211 distinction, packed suffixes, and the deliberate
malformed-status compatibility case.

## What the branch tests prove

The shared fixture is not a parser-only test. Each repository feeds the cases
through its production response boundary and checks the resulting status,
response shape, framed bytes, and retained suffix. The cases deliberately
include every split position for small frames, one-byte fragments, packed
responses, an incomplete following response, empty multiline content, and the
same status code under different request contexts. Article cases additionally
cover dot-stuffed bodies, folded headers, and malformed content.

The nntpbench tests exercise the buffered receiver's complete operation: it
owns the pending bytes and decoder, translates chunk progress internally,
extracts exactly one immutable response, and leaves the suffix for the next
request. They also check the framed-to-validated article transition, repeated
typed access, independently allocated value equality, retained first-line and
transformation metadata, and allocation-free access to plain bodies.

The nntp-proxy tests exercise the streaming framer and its operation-owned
continuation: consumed append permissions cannot be reused, appended bytes
remain tied to the pooled buffer, packed suffixes are separated without a
caller-supplied offset, and write, observe, capture, and cache paths preserve
the same framing result. Storage tests cover exact, one-byte, and roomy tails,
compaction, fresh reads, pool return, EOF, I/O errors, and cancellation. The
article tests verify that validation is tied to the borrowed or owned bytes
and that forwarding keeps wire data borrowed unless a consumer explicitly
requests retention.

These tests assert bytes, suffixes, ownership transitions, and error classes;
they are not satisfied by merely observing that a response parsed. The low-
level scanner tests remain useful oracles, but production-boundary tests are
the evidence for the contracts described above.
