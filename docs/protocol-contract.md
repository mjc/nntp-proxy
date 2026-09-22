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
request-scoped article forms (ARTICLE, HEAD, BODY, and STAT), retains the
parsed first-line metadata and transformation requirements, and keeps the
validated layout tied to its bytes. Plain content can be borrowed; folded
headers and dot-stuffed bodies are materialized only when required. Optional
yEnc validation is a policy at this semantic boundary, not a framing rule.

## Coordinates

All endpoints are exclusive:

| Coordinate | Meaning | Owner |
| --- | --- | --- |
| ChunkConsumed | Count consumed from one scanner push | Framer/decoder |
| FrameEnd | Exclusive end in the accumulated logical response window | Receiver/framer |
| StatusLineEnd | Exclusive end of the request-scoped initial line | Framed article state |
| ContentEnd | Exclusive end before the multiline terminator | Framed article state |

A chunk coordinate is never a frame coordinate. Translation happens once inside
the receiver/framer operation that owns both origins. Newtypes document the
coordinate kind, but they do not identify a buffer; resource identity comes
from the owning state or exclusive borrow.

## Ownership map

### nntpbench adapter

BufferedResponseReceiver owns the pending BytesMut and its request-scoped
decoder. The decoder performs the only split/freeze operation and returns one
framed immutable owner while retaining a packed suffix in the receiver. The
article typestate transition is:

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
