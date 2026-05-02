//! Hybrid cache entry type and foyer codec
//!
//! Contains `HybridArticleEntry` and its manual `Code` implementation for
//! efficient serialization to/from foyer's disk cache.
//!
//! # Wire Format
//!
//! ```text
//! [magic:u32][status:u16][checked:u8][missing:u8][timestamp:u64][tier:u8][payload-kind:u8]...
//! ```

use crate::protocol::RequestKind;
use crate::protocol::StatusCode;
use crate::router::BackendCount;
use crate::types::BackendId;
use foyer::Code;
use std::io::{Read, Write};

use super::article::{CachedArticleNumber, CachedPayload, parse_payload, parse_payload_chunks};
use super::availability::ArticleAvailability;
use super::ttl;

const HYBRID_ENTRY_MAGIC_V3: u32 = 0x4e50_4333; // "NPC3"
const PAYLOAD_MISSING: u8 = 0;
const PAYLOAD_AVAILABILITY_ONLY: u8 = 1;
const PAYLOAD_ARTICLE: u8 = 2;
const PAYLOAD_HEAD: u8 = 3;
const PAYLOAD_BODY: u8 = 4;
const PAYLOAD_STAT: u8 = 5;
const NO_ARTICLE_NUMBER: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CachedSectionLen(u32);

impl CachedSectionLen {
    const MAX: usize = 100 * 1024 * 1024;

    fn try_from_usize(value: usize) -> foyer::Result<Self> {
        let len = u32::try_from(value).map_err(|_| {
            foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Cached article section too large: {value} bytes"),
            ))
        })?;
        Ok(Self(len))
    }

    fn from_wire(value: u32) -> foyer::Result<Self> {
        if value as usize > Self::MAX {
            return Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Cached article section too large: {value} bytes"),
            )));
        }
        Ok(Self(value))
    }

    const fn get(self) -> u32 {
        self.0
    }

    const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Valid NNTP status codes for cached articles
///
/// Using an enum instead of raw `u16` makes invalid states unrepresentable.
/// The `repr(u16)` allows efficient serialization as a 2-byte wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum CacheableStatusCode {
    /// 220 — Full article (headers + body)
    Article = 220,
    /// 221 — Headers only
    Head = 221,
    /// 222 — Body only
    Body = 222,
    /// 223 — Article exists (STAT response)
    Stat = 223,
    /// 430 — Article not found
    Missing = 430,
}

impl CacheableStatusCode {
    /// Get the raw u16 value
    #[inline]
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

impl TryFrom<u16> for CacheableStatusCode {
    type Error = u16;

    fn try_from(code: u16) -> Result<Self, Self::Error> {
        match code {
            220 => Ok(Self::Article),
            221 => Ok(Self::Head),
            222 => Ok(Self::Body),
            223 => Ok(Self::Stat),
            430 => Ok(Self::Missing),
            other => Err(other),
        }
    }
}

/// Cache entry for hybrid storage
///
/// INVARIANT: Every entry has a valid NNTP status code (220, 221, 222, 223, 430).
/// This is enforced at construction - `new()` returns `Option<Self>`.
///
/// Implements foyer's Code trait manually for efficient serialization:
/// - Pre-allocates buffer on decode (no vec resizing)
/// - Simple binary format:
///   [magic:u32][status:u16][checked:u8][missing:u8][timestamp:u64][tier:u8][typed-payload]
#[derive(Clone, Debug)]
pub struct HybridArticleEntry {
    /// Validated NNTP status code — only cacheable codes are representable
    status_code: CacheableStatusCode,
    /// Backend availability tracking (checked/missing bitsets)
    pub(super) availability: ArticleAvailability,
    /// Unix timestamp when availability info was last updated (milliseconds since epoch)
    /// Used to expire stale availability-only entries (430 stubs, STAT responses)
    /// and for tier-aware TTL calculation
    pub(super) timestamp: ttl::CacheTimestampMillis,
    /// Server tier (lower = higher priority)
    /// Used for tier-aware TTL: higher tier = longer TTL
    tier: ttl::CacheTier,
    payload: CachedPayload,
}

/// Manual Code implementation to avoid bincode's vec resizing overhead
impl Code for HybridArticleEntry {
    fn encode(&self, writer: &mut impl Write) -> foyer::Result<()> {
        writer
            .write_all(&HYBRID_ENTRY_MAGIC_V3.to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&self.status_code.as_u16().to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&[
                self.availability.checked_bits(),
                self.availability.missing_bits(),
            ])
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&self.timestamp.get().to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&[self.tier.get()])
            .map_err(foyer::Error::io_error)?;
        encode_payload(writer, &self.payload)?;
        Ok(())
    }

    fn decode(reader: &mut impl Read) -> foyer::Result<Self> {
        let mut magic = [0u8; 4];
        reader
            .read_exact(&mut magic)
            .map_err(foyer::Error::io_error)?;
        let magic = u32::from_le_bytes(magic);
        if magic != HYBRID_ENTRY_MAGIC_V3 {
            return Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "old hybrid cache entry format",
            )));
        }

        // Read status code
        let mut status_bytes = [0u8; 2];
        reader
            .read_exact(&mut status_bytes)
            .map_err(foyer::Error::io_error)?;
        let raw_code = u16::from_le_bytes(status_bytes);

        // Validate status code on decode - reject corrupted entries
        let status_code = CacheableStatusCode::try_from(raw_code).map_err(|code| {
            foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid cached status code: {code}"),
            ))
        })?;

        // Read header: checked + missing
        let mut header = [0u8; 2];
        reader
            .read_exact(&mut header)
            .map_err(foyer::Error::io_error)?;

        // Read timestamp
        let mut timestamp_bytes = [0u8; 8];
        reader
            .read_exact(&mut timestamp_bytes)
            .map_err(foyer::Error::io_error)?;
        let timestamp = ttl::CacheTimestampMillis::new(u64::from_le_bytes(timestamp_bytes));

        // Read tier
        let mut tier_byte = [0u8; 1];
        reader
            .read_exact(&mut tier_byte)
            .map_err(foyer::Error::io_error)?;
        let tier = ttl::CacheTier::new(tier_byte[0]);

        let payload = decode_payload(reader)?;

        Ok(Self {
            status_code,
            availability: ArticleAvailability::from_bits(header[0], header[1]),
            timestamp,
            tier,
            payload,
        })
    }

    fn estimated_size(&self) -> usize {
        4 + 2 + 2 + 8 + 1 + encoded_payload_size(&self.payload)
    }
}

fn encode_payload(writer: &mut impl Write, payload: &CachedPayload) -> foyer::Result<()> {
    match payload {
        CachedPayload::Missing => writer
            .write_all(&[PAYLOAD_MISSING])
            .map_err(foyer::Error::io_error),
        CachedPayload::AvailabilityOnly => writer
            .write_all(&[PAYLOAD_AVAILABILITY_ONLY])
            .map_err(foyer::Error::io_error),
        CachedPayload::Stat { article_number } => {
            writer
                .write_all(&[PAYLOAD_STAT])
                .map_err(foyer::Error::io_error)?;
            write_article_number(writer, *article_number)
        }
        CachedPayload::Article {
            article_number,
            headers,
            body,
        } => {
            writer
                .write_all(&[PAYLOAD_ARTICLE])
                .map_err(foyer::Error::io_error)?;
            write_article_number(writer, *article_number)?;
            write_section(writer, headers)?;
            write_section(writer, body)
        }
        CachedPayload::Head {
            article_number,
            headers,
        } => {
            writer
                .write_all(&[PAYLOAD_HEAD])
                .map_err(foyer::Error::io_error)?;
            write_article_number(writer, *article_number)?;
            write_section(writer, headers)
        }
        CachedPayload::Body {
            article_number,
            body,
        } => {
            writer
                .write_all(&[PAYLOAD_BODY])
                .map_err(foyer::Error::io_error)?;
            write_article_number(writer, *article_number)?;
            write_section(writer, body)
        }
    }
}

fn decode_payload(reader: &mut impl Read) -> foyer::Result<CachedPayload> {
    let mut kind = [0u8; 1];
    reader
        .read_exact(&mut kind)
        .map_err(foyer::Error::io_error)?;
    match kind[0] {
        PAYLOAD_MISSING => Ok(CachedPayload::Missing),
        PAYLOAD_AVAILABILITY_ONLY => Ok(CachedPayload::AvailabilityOnly),
        PAYLOAD_STAT => Ok(CachedPayload::Stat {
            article_number: read_article_number(reader)?,
        }),
        PAYLOAD_ARTICLE => {
            let article_number = read_article_number(reader)?;
            let headers = read_section(reader)?;
            let body = read_section(reader)?;
            Ok(CachedPayload::Article {
                article_number,
                headers,
                body,
            })
        }
        PAYLOAD_HEAD => {
            let article_number = read_article_number(reader)?;
            let headers = read_section(reader)?;
            Ok(CachedPayload::Head {
                article_number,
                headers,
            })
        }
        PAYLOAD_BODY => {
            let article_number = read_article_number(reader)?;
            let body = read_section(reader)?;
            Ok(CachedPayload::Body {
                article_number,
                body,
            })
        }
        other => Err(foyer::Error::io_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid cached payload kind: {other}"),
        ))),
    }
}

fn write_article_number(
    writer: &mut impl Write,
    article_number: Option<CachedArticleNumber>,
) -> foyer::Result<()> {
    writer
        .write_all(
            &article_number
                .map_or(NO_ARTICLE_NUMBER, CachedArticleNumber::get)
                .to_le_bytes(),
        )
        .map_err(foyer::Error::io_error)
}

fn read_article_number(reader: &mut impl Read) -> foyer::Result<Option<CachedArticleNumber>> {
    let mut bytes = [0u8; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(foyer::Error::io_error)?;
    let raw = u64::from_le_bytes(bytes);
    Ok((raw != NO_ARTICLE_NUMBER).then(|| CachedArticleNumber::new(raw)))
}

fn write_section(writer: &mut impl Write, data: &[u8]) -> foyer::Result<()> {
    let len = CachedSectionLen::try_from_usize(data.len())?;
    writer
        .write_all(&len.get().to_le_bytes())
        .map_err(foyer::Error::io_error)?;
    writer.write_all(data).map_err(foyer::Error::io_error)
}

fn read_section(reader: &mut impl Read) -> foyer::Result<Box<[u8]>> {
    let mut len_bytes = [0u8; 4];
    reader
        .read_exact(&mut len_bytes)
        .map_err(foyer::Error::io_error)?;
    let len = CachedSectionLen::from_wire(u32::from_le_bytes(len_bytes))?.as_usize();
    let mut data = Vec::with_capacity(len);
    reader
        .take(len as u64)
        .read_to_end(&mut data)
        .map_err(foyer::Error::io_error)?;
    if data.len() != len {
        return Err(foyer::Error::io_error(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("Expected {} bytes, got {}", len, data.len()),
        )));
    }
    Ok(data.into_boxed_slice())
}

fn encoded_payload_size(payload: &CachedPayload) -> usize {
    match payload {
        CachedPayload::Missing | CachedPayload::AvailabilityOnly => 1,
        CachedPayload::Stat { .. } => 1 + 8,
        CachedPayload::Article { headers, body, .. } => 1 + 8 + 4 + headers.len() + 4 + body.len(),
        CachedPayload::Head { headers, .. } => 1 + 8 + 4 + headers.len(),
        CachedPayload::Body { body, .. } => 1 + 8 + 4 + body.len(),
    }
}

impl HybridArticleEntry {
    /// Ingest a cold wire response into a typed hybrid cache entry.
    ///
    /// Returns `None` if the status code is invalid or not cacheable. The entry
    /// stores semantic payload sections, not the original response.
    #[must_use]
    pub fn from_wire_response(buffer: impl AsRef<[u8]>) -> Option<Self> {
        Self::from_wire_response_with_tier(buffer, ttl::CacheTier::new(0))
    }

    /// Ingest a cold wire response with a specific provider tier.
    #[must_use]
    pub fn from_wire_response_with_tier(
        buffer: impl AsRef<[u8]>,
        tier: ttl::CacheTier,
    ) -> Option<Self> {
        let buffer = buffer.as_ref();
        let raw_code = StatusCode::parse(buffer)?.as_u16();
        let status_code = CacheableStatusCode::try_from(raw_code).ok()?;
        let payload = parse_payload(StatusCode::new(raw_code), buffer);

        Some(Self {
            status_code,
            availability: ArticleAvailability::new(),
            timestamp: ttl::CacheTimestampMillis::now(),
            tier,
            payload,
        })
    }

    #[must_use]
    pub(crate) fn from_cache_buffer_with_tier(
        buffer: super::CacheBuffer,
        tier: ttl::CacheTier,
    ) -> Option<Self> {
        match buffer {
            super::CacheBuffer::Boxed(buffer) => Self::from_wire_response_with_tier(buffer, tier),
            super::CacheBuffer::Pooled(buffer) => {
                Self::from_wire_response_with_tier(buffer.as_ref(), tier)
            }
            super::CacheBuffer::Chunked(buffer) => {
                Self::from_chunked_response_with_tier(&buffer, tier)
            }
            super::CacheBuffer::Small(buffer) => Self::from_wire_response_with_tier(buffer, tier),
        }
    }

    #[must_use]
    fn from_chunked_response_with_tier(
        buffer: &crate::pool::ChunkedResponse,
        tier: ttl::CacheTier,
    ) -> Option<Self> {
        let mut prefix = smallvec::SmallVec::<[u8; 128]>::new();
        buffer.copy_prefix_into(3, &mut prefix);
        let raw_code = StatusCode::parse(&prefix)?.as_u16();
        let status_code = CacheableStatusCode::try_from(raw_code).ok()?;
        let payload = parse_payload_chunks(StatusCode::new(raw_code), buffer.iter_chunks());

        Some(Self {
            status_code,
            availability: ArticleAvailability::new(),
            timestamp: ttl::CacheTimestampMillis::now(),
            tier,
            payload,
        })
    }

    #[must_use]
    pub fn availability_only(status_code: CacheableStatusCode, tier: ttl::CacheTier) -> Self {
        Self {
            status_code,
            availability: ArticleAvailability::new(),
            timestamp: ttl::CacheTimestampMillis::now(),
            tier,
            payload: CachedPayload::AvailabilityOnly,
        }
    }

    #[must_use]
    pub fn response_parts_for_request_kind(
        &self,
        request_kind: RequestKind,
        message_id: &str,
    ) -> Option<super::article::CachedArticleResponse<'_>> {
        super::article::response_parts_for_payload_kind(&self.payload, request_kind, message_id)
    }

    #[must_use]
    pub(crate) fn payload_len(&self) -> super::article::CachedPayloadLen {
        self.payload.len()
    }

    #[must_use]
    pub(crate) fn to_article_entry(&self) -> super::article::ArticleEntry {
        super::article::ArticleEntry::from_parts(
            StatusCode::new(self.status_code.as_u16()),
            self.payload.clone(),
            self.availability,
            self.tier,
            self.timestamp.get(),
        )
    }

    #[inline]
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        StatusCode::new(self.status_code.as_u16())
    }

    /// Check if we should try fetching from this backend
    #[inline]
    #[must_use]
    pub fn should_try_backend(&self, backend_id: BackendId) -> bool {
        self.availability.should_try(backend_id)
    }

    /// Record that a backend returned 430 (doesn't have this article)
    pub fn record_backend_missing(&mut self, backend_id: BackendId) {
        self.availability.record_missing(backend_id);
    }

    /// Record that a backend successfully provided this article
    pub fn record_backend_has(&mut self, backend_id: BackendId) {
        self.availability.record_has(backend_id);
    }

    /// Record successful backend availability without storing response payload bytes.
    pub(super) fn record_backend_has_status(
        &mut self,
        status_code: CacheableStatusCode,
        backend_id: BackendId,
        tier: ttl::CacheTier,
    ) {
        if !self.is_complete_article() {
            self.status_code = status_code;
            self.payload = CachedPayload::AvailabilityOnly;
            self.tier = tier;
        }
        self.timestamp = ttl::CacheTimestampMillis::now();
        self.record_backend_has(backend_id);
    }

    /// Check if all backends have been tried and none have the article
    #[must_use]
    pub fn all_backends_exhausted(&self, total_backends: BackendCount) -> bool {
        self.availability.all_exhausted(total_backends)
    }

    /// Check if this cache entry contains a complete article (220) or body (222)
    #[inline]
    #[must_use]
    pub fn is_complete_article(&self) -> bool {
        matches!(
            (&self.payload, self.status_code.as_u16()),
            (CachedPayload::Article { headers, body, .. }, 220)
                if !headers.is_empty() || !body.is_empty()
        ) || matches!(
            (&self.payload, self.status_code.as_u16()),
            (CachedPayload::Body { body, .. }, 222) if !body.is_empty()
        )
    }

    /// Check if buffer contains a valid NNTP multiline response
    #[inline]
    #[must_use]
    pub fn is_valid_response(&self) -> bool {
        matches!(
            self.payload,
            CachedPayload::Article { .. }
                | CachedPayload::Head { .. }
                | CachedPayload::Body { .. }
                | CachedPayload::Stat { .. }
                | CachedPayload::Missing
                | CachedPayload::AvailabilityOnly
        )
    }

    /// Get backend availability as `ArticleAvailability` struct
    #[inline]
    #[must_use]
    pub const fn availability(&self) -> ArticleAvailability {
        self.availability
    }

    /// Check if we have any backend availability information
    ///
    /// Returns true if at least one backend has been checked.
    /// Wrapper around `ArticleAvailability::has_availability_info()` for convenience.
    #[inline]
    #[must_use]
    pub const fn has_availability_info(&self) -> bool {
        self.availability.has_availability_info()
    }

    /// Check if availability information is stale (older than `ttl_millis`)
    ///
    /// `HybridArticleEntry` stores timestamps for tier-aware TTL, but foyer's cache
    /// handles eviction based on insertion time. This method is kept for compatibility
    /// and always returns false since the cache layer manages staleness.
    #[inline]
    #[must_use]
    pub const fn is_availability_stale(&self, _ttl_millis: u64) -> bool {
        // Foyer cache handles TTL-based eviction separately
        false
    }

    /// Clear stale availability information
    ///
    /// `HybridArticleEntry` now tracks timestamps via `timestamp` field for tier-aware TTL,
    /// but foyer handles eviction separately. This method is a no-op for compatibility.
    #[inline]
    pub const fn clear_stale_availability(&mut self, _ttl_millis: u64) {
        // Foyer cache handles TTL-based eviction, no need to clear here
    }

    /// Check if this entry has expired based on tier-aware TTL
    ///
    /// See [`super::ttl`] for the TTL formula.
    #[inline]
    #[must_use]
    pub fn is_expired(&self, base_ttl_millis: u64) -> bool {
        ttl::is_expired(self.timestamp.get(), base_ttl_millis, self.tier)
    }

    /// Get the tier of the backend that provided this article
    #[inline]
    #[must_use]
    pub const fn tier(&self) -> ttl::CacheTier {
        self.tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendId;
    use futures::executor::block_on;

    fn assert_entry_eq(original: &HybridArticleEntry, decoded: &HybridArticleEntry) {
        assert_eq!(original.status_code, decoded.status_code);
        assert_eq!(original.availability, decoded.availability);
        assert_eq!(original.timestamp, decoded.timestamp);
        assert_eq!(original.tier, decoded.tier);
        assert_eq!(original.payload, decoded.payload);
    }

    fn render_response(
        entry: &HybridArticleEntry,
        verb: &[u8],
        message_id: &str,
    ) -> Option<Vec<u8>> {
        let response = entry.response_parts_for_request_kind(verb_to_kind(verb)?, message_id)?;
        let mut out = Vec::with_capacity(response.wire_len().get());
        block_on(response.write_to(&mut out)).ok()?;
        Some(out)
    }

    fn verb_to_kind(verb: &[u8]) -> Option<RequestKind> {
        match verb {
            verb if verb.eq_ignore_ascii_case(b"ARTICLE") => Some(RequestKind::Article),
            verb if verb.eq_ignore_ascii_case(b"HEAD") => Some(RequestKind::Head),
            verb if verb.eq_ignore_ascii_case(b"BODY") => Some(RequestKind::Body),
            verb if verb.eq_ignore_ascii_case(b"STAT") => Some(RequestKind::Stat),
            _ => None,
        }
    }

    // =========================================================================
    // CacheableStatusCode enum tests
    // =========================================================================

    #[test]
    fn test_cacheable_status_code_as_u16() {
        assert_eq!(CacheableStatusCode::Article.as_u16(), 220);
        assert_eq!(CacheableStatusCode::Head.as_u16(), 221);
        assert_eq!(CacheableStatusCode::Body.as_u16(), 222);
        assert_eq!(CacheableStatusCode::Stat.as_u16(), 223);
        assert_eq!(CacheableStatusCode::Missing.as_u16(), 430);
    }

    #[test]
    fn test_cacheable_status_code_try_from_valid() {
        assert_eq!(
            CacheableStatusCode::try_from(220),
            Ok(CacheableStatusCode::Article)
        );
        assert_eq!(
            CacheableStatusCode::try_from(221),
            Ok(CacheableStatusCode::Head)
        );
        assert_eq!(
            CacheableStatusCode::try_from(222),
            Ok(CacheableStatusCode::Body)
        );
        assert_eq!(
            CacheableStatusCode::try_from(223),
            Ok(CacheableStatusCode::Stat)
        );
        assert_eq!(
            CacheableStatusCode::try_from(430),
            Ok(CacheableStatusCode::Missing)
        );
    }

    #[test]
    fn test_cacheable_status_code_try_from_invalid() {
        assert_eq!(CacheableStatusCode::try_from(219), Err(219));
        assert_eq!(CacheableStatusCode::try_from(224), Err(224));
        assert_eq!(CacheableStatusCode::try_from(429), Err(429));
        assert_eq!(CacheableStatusCode::try_from(431), Err(431));
        assert_eq!(CacheableStatusCode::try_from(200), Err(200));
        assert_eq!(CacheableStatusCode::try_from(201), Err(201));
        assert_eq!(CacheableStatusCode::try_from(211), Err(211));
        assert_eq!(CacheableStatusCode::try_from(411), Err(411));
        assert_eq!(CacheableStatusCode::try_from(480), Err(480));
        assert_eq!(CacheableStatusCode::try_from(500), Err(500));
        assert_eq!(CacheableStatusCode::try_from(0), Err(0));
        assert_eq!(CacheableStatusCode::try_from(u16::MAX), Err(u16::MAX));
    }

    #[test]
    fn test_cacheable_status_code_roundtrip() {
        for code in [
            CacheableStatusCode::Article,
            CacheableStatusCode::Head,
            CacheableStatusCode::Body,
            CacheableStatusCode::Stat,
            CacheableStatusCode::Missing,
        ] {
            let raw = code.as_u16();
            let back = CacheableStatusCode::try_from(raw).unwrap();
            assert_eq!(code, back);
        }
    }

    #[test]
    fn test_cacheable_status_code_clone_copy() {
        let a = CacheableStatusCode::Article;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_cacheable_status_code_debug() {
        let dbg = format!("{:?}", CacheableStatusCode::Article);
        assert!(dbg.contains("Article"));
        let dbg = format!("{:?}", CacheableStatusCode::Missing);
        assert!(dbg.contains("Missing"));
    }

    #[test]
    fn test_cacheable_status_code_eq() {
        assert_eq!(CacheableStatusCode::Article, CacheableStatusCode::Article);
        assert_ne!(CacheableStatusCode::Article, CacheableStatusCode::Body);
        assert_ne!(CacheableStatusCode::Head, CacheableStatusCode::Missing);
    }

    #[test]
    fn test_cacheable_status_code_repr_u16_size() {
        use std::mem::size_of;
        assert_eq!(size_of::<CacheableStatusCode>(), size_of::<u16>());
    }

    #[test]
    fn test_cached_section_len_rejects_oversized_sections() {
        assert_eq!(
            CachedSectionLen::try_from_usize(u32::MAX as usize)
                .unwrap()
                .get(),
            u32::MAX
        );
        assert!(CachedSectionLen::try_from_usize(u32::MAX as usize + 1).is_err());
    }

    // =========================================================================
    // HybridArticleEntry tests
    // =========================================================================

    #[test]
    fn test_hybrid_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let mut entry =
            HybridArticleEntry::from_wire_response(buffer.clone()).expect("valid status code");

        assert_eq!(
            render_response(&entry, b"ARTICLE", "<test@example.com>").unwrap(),
            buffer
        );
        assert_eq!(entry.status_code().as_u16(), 220);

        entry.record_backend_has(BackendId::from_index(0));
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));

        entry.record_backend_missing(BackendId::from_index(1));
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(!entry.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn hybrid_entry_ingests_wire_response_by_name() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        )
        .expect("valid status code");

        assert_eq!(entry.status_code().as_u16(), 220);
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn hybrid_entry_ingests_borrowed_wire_response_bytes() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".as_slice(),
        )
        .expect("valid status code");

        assert_eq!(entry.status_code().as_u16(), 220);
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn hybrid_entry_ingests_cache_buffer_without_required_vec() {
        let entry = HybridArticleEntry::from_cache_buffer_with_tier(
            smallvec::SmallVec::<[u8; 128]>::from_slice(
                b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
            )
            .into(),
            ttl::CacheTier::new(0),
        )
        .expect("valid status code");

        assert_eq!(entry.status_code().as_u16(), 220);
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn hybrid_entry_ingests_chunked_cache_buffer_without_flattening_wire_response() {
        let pool = crate::pool::BufferPool::new(
            crate::types::BufferSize::try_new(1024).expect("valid buffer size"),
            1,
        )
        .with_capture_pool(8, 4);
        let mut response = crate::pool::ChunkedResponse::default();
        response.extend_from_slice(
            &pool,
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );
        assert!(
            response.iter_chunks().count() > 1,
            "test response must span chunks"
        );

        let entry = HybridArticleEntry::from_cache_buffer_with_tier(
            response.into(),
            ttl::CacheTier::new(0),
        )
        .expect("valid status code");

        assert_eq!(entry.status_code().as_u16(), 220);
        match entry.payload {
            CachedPayload::Article { headers, body, .. } => {
                assert_eq!(headers.as_ref(), b"Subject: Test");
                assert_eq!(body.as_ref(), b"Body");
            }
            other => panic!("expected article payload, got {other:?}"),
        }
    }

    #[test]
    fn test_hybrid_entry_response_parts_do_not_clone_payload() {
        let buffer = b"220 7 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry =
            HybridArticleEntry::from_wire_response(buffer.clone()).expect("valid status code");

        let response = entry
            .response_parts_for_request_kind(RequestKind::Head, "<test@example.com>")
            .expect("article cache entry can serve HEAD");

        let mut rendered = Vec::with_capacity(response.wire_len().get());
        block_on(response.write_to(&mut rendered)).unwrap();
        assert_eq!(
            rendered,
            b"221 7 <test@example.com>\r\nSubject: Test\r\n.\r\n"
        );
    }

    #[test]
    fn test_hybrid_entry_availability() {
        let mut entry = HybridArticleEntry::from_wire_response(b"220 ok\r\n").expect("valid");

        for i in 0..8 {
            assert!(entry.should_try_backend(BackendId::from_index(i)));
        }

        entry.record_backend_missing(BackendId::from_index(0));
        entry.record_backend_missing(BackendId::from_index(2));
        entry.record_backend_missing(BackendId::from_index(4));

        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
        assert!(!entry.should_try_backend(BackendId::from_index(2)));
        assert!(entry.should_try_backend(BackendId::from_index(3)));
        assert!(!entry.should_try_backend(BackendId::from_index(4)));

        let avail = entry.availability();
        assert!(avail.is_missing(BackendId::from_index(0)));
        assert!(!avail.is_missing(BackendId::from_index(1)));
    }

    #[test]
    fn test_hybrid_entry_command_matching() {
        let article =
            HybridArticleEntry::from_wire_response(b"220 0 <id>\r\nH: V\r\n\r\nB\r\n.\r\n")
                .expect("valid");
        assert!(
            article
                .response_parts_for_request_kind(RequestKind::Article, "<id>")
                .is_some()
        );
        assert!(
            article
                .response_parts_for_request_kind(RequestKind::Body, "<id>")
                .is_some()
        );
        assert!(
            article
                .response_parts_for_request_kind(RequestKind::Head, "<id>")
                .is_some()
        );

        let body =
            HybridArticleEntry::from_wire_response(b"222 0 <id>\r\nB\r\n.\r\n").expect("valid");
        assert!(
            body.response_parts_for_request_kind(RequestKind::Article, "<id>")
                .is_none()
        );
        assert!(
            body.response_parts_for_request_kind(RequestKind::Body, "<id>")
                .is_some()
        );
        assert!(
            body.response_parts_for_request_kind(RequestKind::Head, "<id>")
                .is_none()
        );

        let head =
            HybridArticleEntry::from_wire_response(b"221 0 <id>\r\nH: V\r\n.\r\n").expect("valid");
        assert!(
            head.response_parts_for_request_kind(RequestKind::Article, "<id>")
                .is_none()
        );
        assert!(
            head.response_parts_for_request_kind(RequestKind::Body, "<id>")
                .is_none()
        );
        assert!(
            head.response_parts_for_request_kind(RequestKind::Head, "<id>")
                .is_some()
        );
    }

    #[test]
    fn test_hybrid_entry_rejects_invalid() {
        assert!(HybridArticleEntry::from_wire_response(b"999 invalid\r\n").is_none());
        assert!(HybridArticleEntry::from_wire_response(vec![]).is_none());
        assert!(HybridArticleEntry::from_wire_response(b"20").is_none());
        assert!(HybridArticleEntry::from_wire_response(b"abc\r\n").is_none());

        assert!(HybridArticleEntry::from_wire_response(b"220 article\r\n").is_some());
        assert!(HybridArticleEntry::from_wire_response(b"221 head\r\n").is_some());
        assert!(HybridArticleEntry::from_wire_response(b"222 body\r\n").is_some());
        assert!(HybridArticleEntry::from_wire_response(b"223 stat\r\n").is_some());
        assert!(HybridArticleEntry::from_wire_response(b"430 not found\r\n").is_some());
    }

    // =========================================================================
    // Entry status_code field uses enum
    // =========================================================================

    #[test]
    fn test_entry_status_code_returns_protocol_status_code() {
        let entry = HybridArticleEntry::from_wire_response(b"220 0 <id>\r\n").unwrap();
        let sc = entry.status_code();
        assert_eq!(sc.as_u16(), 220);

        let entry = HybridArticleEntry::from_wire_response(b"430 not found\r\n").unwrap();
        let sc = entry.status_code();
        assert_eq!(sc.as_u16(), 430);
    }

    #[test]
    fn test_entry_each_cacheable_code() {
        let cases: &[(&[u8], u16)] = &[
            (b"220 article\r\n", 220),
            (b"221 head\r\n", 221),
            (b"222 body\r\n", 222),
            (b"223 stat\r\n", 223),
            (b"430 missing\r\n", 430),
        ];
        for (buf, expected) in cases {
            let entry = HybridArticleEntry::from_wire_response(buf)
                .unwrap_or_else(|| panic!("should accept code {expected}"));
            assert_eq!(entry.status_code().as_u16(), *expected);
        }
    }

    #[test]
    fn test_entry_rejects_non_cacheable_nntp_codes() {
        for code in [200, 201, 211, 411, 480, 500, 502] {
            let buf = format!("{code} response\r\n").into_bytes();
            assert!(
                HybridArticleEntry::from_wire_response(buf).is_none(),
                "code {code} should be rejected"
            );
        }
    }

    // =========================================================================
    // Code encode/decode roundtrip
    // =========================================================================

    #[test]
    fn test_code_encode_decode_roundtrip_article() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n",
        )
        .unwrap();
        let mut buf = Vec::new();
        entry.encode(&mut buf).unwrap();
        let decoded = HybridArticleEntry::decode(&mut buf.as_slice()).unwrap();

        assert_eq!(decoded.status_code().as_u16(), 220);
        assert_entry_eq(&entry, &decoded);
    }

    #[test]
    fn test_code_encode_decode_roundtrip_all_codes() {
        let buffers: &[&[u8]] = &[
            b"220 article\r\n",
            b"221 head\r\n",
            b"222 body\r\n",
            b"223 stat\r\n",
            b"430 missing\r\n",
        ];
        for raw in buffers {
            let entry = HybridArticleEntry::from_wire_response(raw).unwrap();
            let mut encoded = Vec::new();
            entry.encode(&mut encoded).unwrap();
            let decoded = HybridArticleEntry::decode(&mut encoded.as_slice()).unwrap();
            assert_eq!(decoded.status_code().as_u16(), entry.status_code().as_u16());
            assert_entry_eq(&entry, &decoded);
        }
    }

    #[test]
    fn test_code_decode_rejects_invalid_status() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&999u16.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.push(0);
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(b"hello");

        let result = HybridArticleEntry::decode(&mut buf.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_code_encode_decode_preserves_tier() {
        let entry = HybridArticleEntry::from_wire_response_with_tier(
            b"220 article\r\n",
            ttl::CacheTier::new(3),
        )
        .unwrap();
        assert_eq!(entry.tier().get(), 3);

        let mut encoded = Vec::new();
        entry.encode(&mut encoded).unwrap();
        let decoded = HybridArticleEntry::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.tier().get(), 3);
    }

    #[test]
    fn test_code_encode_decode_preserves_availability() {
        let mut entry = HybridArticleEntry::from_wire_response(b"220 ok\r\n").unwrap();
        entry.record_backend_has(BackendId::from_index(0));
        entry.record_backend_missing(BackendId::from_index(2));

        let mut encoded = Vec::new();
        entry.encode(&mut encoded).unwrap();
        let decoded = HybridArticleEntry::decode(&mut encoded.as_slice()).unwrap();

        assert!(decoded.should_try_backend(BackendId::from_index(0)));
        assert!(decoded.should_try_backend(BackendId::from_index(1)));
        assert!(!decoded.should_try_backend(BackendId::from_index(2)));
    }

    #[test]
    fn test_code_estimated_size() {
        let entry = HybridArticleEntry::from_wire_response(b"220 article\r\n").unwrap();
        let expected = 4 + 2 + 2 + 8 + 1 + 1;
        assert_eq!(entry.estimated_size(), expected);
    }

    // =========================================================================
    // is_complete_article
    // =========================================================================

    #[test]
    fn test_is_complete_article_220() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n",
        )
        .unwrap();
        assert!(entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_222() {
        let entry =
            HybridArticleEntry::from_wire_response(b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n")
                .unwrap();
        assert!(entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_head() {
        let entry =
            HybridArticleEntry::from_wire_response(b"221 0 <t@x>\r\nSubject: T\r\n.\r\n").unwrap();
        assert!(!entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_stat() {
        let entry = HybridArticleEntry::from_wire_response(b"223 0 <t@x>\r\n").unwrap();
        assert!(!entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_430() {
        let entry = HybridArticleEntry::from_wire_response(b"430 not found\r\n").unwrap();
        assert!(!entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_too_small_buffer() {
        let entry = HybridArticleEntry::from_wire_response(b"220 ok\r\n.\r\n").unwrap();
        assert!(!entry.is_complete_article());
    }

    // =========================================================================
    // response_for_command
    // =========================================================================

    #[test]
    fn test_response_for_command_stat_from_220() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n",
        )
        .unwrap();
        let resp = render_response(&entry, b"STAT", "<t@x>").expect("should serve STAT");
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_response_for_command_stat_from_221() {
        let entry =
            HybridArticleEntry::from_wire_response(b"221 0 <t@x>\r\nSubject: T\r\n.\r\n").unwrap();
        let resp = render_response(&entry, b"STAT", "<t@x>").expect("should serve STAT from head");
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_response_for_command_stat_from_222() {
        let entry =
            HybridArticleEntry::from_wire_response(b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n")
                .unwrap();
        let resp = render_response(&entry, b"STAT", "<t@x>").expect("should serve STAT from body");
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_response_for_command_stat_not_from_430() {
        let entry = HybridArticleEntry::from_wire_response(b"430 not found\r\n").unwrap();
        assert!(render_response(&entry, b"STAT", "<t@x>").is_none());
    }

    #[test]
    fn test_response_for_command_article_direct() {
        use crate::protocol::RequestKind;

        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = HybridArticleEntry::from_wire_response(buf.clone()).unwrap();
        let resp = render_response(&entry, b"ARTICLE", "<t@x>").expect("should serve ARTICLE");
        assert_eq!(resp, buf);

        let response = entry
            .response_parts_for_request_kind(RequestKind::Article, "<t@x>")
            .expect("should serve ARTICLE by request kind");
        let mut out = Vec::with_capacity(response.wire_len().get());
        block_on(response.write_to(&mut out)).unwrap();
        assert_eq!(out, buf);
    }

    #[test]
    fn test_response_for_command_body_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = HybridArticleEntry::from_wire_response(buf.clone()).unwrap();
        let resp = render_response(&entry, b"BODY", "<t@x>").expect("220 can serve BODY");
        assert_eq!(resp, b"222 0 <t@x>\r\nBody\r\n.\r\n");
    }

    #[test]
    fn test_response_for_command_head_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = HybridArticleEntry::from_wire_response(buf.clone()).unwrap();
        let resp = render_response(&entry, b"HEAD", "<t@x>").expect("220 can serve HEAD");
        assert_eq!(resp, b"221 0 <t@x>\r\nSubject: T\r\n.\r\n");
    }

    #[test]
    fn test_response_for_command_body_cannot_serve_article() {
        let entry =
            HybridArticleEntry::from_wire_response(b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n")
                .unwrap();
        assert!(render_response(&entry, b"ARTICLE", "<t@x>").is_none());
    }

    #[test]
    fn test_response_for_command_head_cannot_serve_body() {
        let entry =
            HybridArticleEntry::from_wire_response(b"221 0 <t@x>\r\nSubject: T\r\n.\r\n").unwrap();
        assert!(render_response(&entry, b"BODY", "<t@x>").is_none());
    }

    #[test]
    fn test_response_for_command_unknown_verb() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n",
        )
        .unwrap();
        assert!(render_response(&entry, b"LIST", "<t@x>").is_none());
        assert!(render_response(&entry, b"GROUP", "<t@x>").is_none());
        assert!(render_response(&entry, b"QUIT", "<t@x>").is_none());
    }

    #[test]
    fn test_with_tier_sets_tier() {
        let entry =
            HybridArticleEntry::from_wire_response_with_tier(b"220 ok\r\n", ttl::CacheTier::new(5))
                .unwrap();
        assert_eq!(entry.tier().get(), 5);
    }

    #[test]
    fn test_with_tier_zero_default() {
        let entry = HybridArticleEntry::from_wire_response(b"220 ok\r\n").unwrap();
        assert_eq!(entry.tier().get(), 0);
    }

    #[test]
    fn test_with_tier_rejects_invalid_code() {
        assert!(
            HybridArticleEntry::from_wire_response_with_tier(
                b"999 bad\r\n",
                ttl::CacheTier::new(0)
            )
            .is_none()
        );
    }

    // =========================================================================
    // Property tests for HybridArticleEntry codec
    // =========================================================================

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_220() {
        let original = HybridArticleEntry::from_wire_response(
            b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n",
        )
        .unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_entry_eq(&original, &decoded);
        assert_eq!(original.tier().get(), decoded.tier().get());
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_221() {
        let original = HybridArticleEntry::from_wire_response(
            b"221 headers\r\nMid: <test@example.com>\r\n\r\n.\r\n",
        )
        .unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_entry_eq(&original, &decoded);
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_222() {
        let original =
            HybridArticleEntry::from_wire_response(b"222 body\r\n\r\nbody content\r\n.\r\n")
                .unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_entry_eq(&original, &decoded);
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_223() {
        let original = HybridArticleEntry::from_wire_response(b"223 stat\r\n.\r\n").unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_entry_eq(&original, &decoded);
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_430() {
        let original = HybridArticleEntry::from_wire_response(b"430 missing\r\n.\r\n").unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_entry_eq(&original, &decoded);
    }

    #[test]
    fn prop_hybrid_article_estimated_size_matches_encoded() {
        let codes: Vec<&[u8]> = vec![
            b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n",
            b"221 headers\r\nMid: <test@example.com>\r\n\r\n.\r\n",
            b"222 body\r\nbody content\r\n.\r\n",
            b"223 stat\r\n.\r\n",
            b"430 missing\r\n.\r\n",
        ];

        for code in &codes {
            let entry = HybridArticleEntry::from_wire_response(code).unwrap();
            let estimated = entry.estimated_size();

            let mut buffer = Vec::new();
            entry.encode(&mut buffer).unwrap();

            assert_eq!(
                estimated,
                buffer.len(),
                "estimated_size mismatch for {:?}",
                std::str::from_utf8(code)
            );
        }
    }

    #[test]
    fn prop_hybrid_article_decode_rejects_invalid_status_code() {
        // Create a valid encoding but with an invalid status code
        let mut buffer = Vec::new();

        // Write invalid status code (500)
        buffer.extend_from_slice(&500u16.to_le_bytes());
        // Write dummy header bytes
        buffer.extend_from_slice(&[0u8, 0u8]);
        // Write dummy timestamp
        buffer.extend_from_slice(&0u64.to_le_bytes());
        // Write dummy tier
        buffer.push(0u8);
        // Write dummy length
        buffer.extend_from_slice(&0u32.to_le_bytes());

        let mut reader = std::io::Cursor::new(buffer);
        let result = HybridArticleEntry::decode(&mut reader);

        assert!(result.is_err(), "Should reject invalid status code 500");
    }

    #[test]
    fn prop_hybrid_article_preserves_tier() {
        for tier in [0u8, 1, 5, 10, 255] {
            let entry = HybridArticleEntry::from_wire_response_with_tier(
                b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n",
                ttl::CacheTier::new(tier),
            )
            .unwrap();

            let mut buffer = Vec::new();
            entry.encode(&mut buffer).unwrap();

            let mut reader = std::io::Cursor::new(buffer);
            let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

            assert_eq!(tier, decoded.tier().get(), "Tier mismatch");
        }
    }

    #[test]
    fn prop_hybrid_article_preserves_availability() {
        let entry = HybridArticleEntry::from_wire_response(
            b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n",
        )
        .unwrap();

        let mut buffer = Vec::new();
        entry.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        // Compare the bitset representation
        assert_eq!(
            entry.availability.checked_bits(),
            decoded.availability.checked_bits(),
            "Checked bits mismatch"
        );
        assert_eq!(
            entry.availability.missing_bits(),
            decoded.availability.missing_bits(),
            "Missing bits mismatch"
        );
    }
}
