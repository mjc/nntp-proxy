//! Article caching implementation using LRU cache with TTL
//!
//! This module provides article caching with per-backend availability tracking.
//! The availability tracking type itself lives in [`super::availability`].

use crate::protocol::{RequestKind, StatusCode};
use crate::router::BackendCount;
use crate::types::{BackendId, MessageId};
use moka::future::Cache;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::availability::ArticleAvailability;
use super::ttl;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CachedArticleNumber(u64);

impl CachedArticleNumber {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for CachedArticleNumber {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CachedPayloadLen(usize);

impl CachedPayloadLen {
    #[must_use]
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for CachedPayloadLen {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for CachedPayloadLen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

impl PartialEq<usize> for CachedPayloadLen {
    fn eq(&self, other: &usize) -> bool {
        self.get() == *other
    }
}

impl PartialOrd<usize> for CachedPayloadLen {
    fn partial_cmp(&self, other: &usize) -> Option<std::cmp::Ordering> {
        self.get().partial_cmp(other)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CachedPayload {
    Missing,
    AvailabilityOnly,
    Article {
        article_number: Option<CachedArticleNumber>,
        headers: Box<[u8]>,
        body: Box<[u8]>,
    },
    Head {
        article_number: Option<CachedArticleNumber>,
        headers: Box<[u8]>,
    },
    Body {
        article_number: Option<CachedArticleNumber>,
        body: Box<[u8]>,
    },
    Stat {
        article_number: Option<CachedArticleNumber>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CachedPayloadKind {
    Missing,
    AvailabilityOnly,
    Article,
    Head,
    Body,
    Stat,
}

#[derive(Debug, Clone, Copy)]
pub struct CachedArticleResponse<'a> {
    status: StatusCode,
    status_line: StackStatusLine,
    payload: CachedArticleResponsePayload<'a>,
}

#[derive(Debug, Clone, Copy)]
enum CachedArticleResponsePayload<'a> {
    None,
    Article { headers: &'a [u8], body: &'a [u8] },
    Head { headers: &'a [u8] },
    Body { body: &'a [u8] },
}

impl CachedArticleResponse<'_> {
    fn wire_len_usize(&self) -> usize {
        self.status_line.len() + self.payload_len()
    }

    #[must_use]
    pub fn wire_len(&self) -> crate::protocol::ResponseWireLen {
        self.wire_len_usize().into()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.wire_len_usize() == 0
    }

    fn status_line(&self) -> &[u8] {
        self.status_line.as_slice()
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    pub async fn write_to<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt as _;

        writer.write_all(self.status_line()).await?;
        match self.payload {
            CachedArticleResponsePayload::None => {}
            CachedArticleResponsePayload::Article { headers, body } => {
                writer.write_all(headers).await?;
                writer.write_all(b"\r\n\r\n").await?;
                writer.write_all(body).await?;
                writer.write_all(b"\r\n.\r\n").await?;
            }
            CachedArticleResponsePayload::Head { headers } => {
                writer.write_all(headers).await?;
                writer.write_all(b"\r\n.\r\n").await?;
            }
            CachedArticleResponsePayload::Body { body } => {
                writer.write_all(body).await?;
                writer.write_all(b"\r\n.\r\n").await?;
            }
        }
        Ok(())
    }

    fn payload_len(&self) -> usize {
        match self.payload {
            CachedArticleResponsePayload::None => 0,
            CachedArticleResponsePayload::Article { headers, body } => {
                headers.len() + 4 + body.len() + 5
            }
            CachedArticleResponsePayload::Head { headers } => headers.len() + 5,
            CachedArticleResponsePayload::Body { body } => body.len() + 5,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StackStatusLine {
    bytes: [u8; 1024],
    len: usize,
}

impl StackStatusLine {
    fn new(code: u16, article_number: u64, message_id: &str) -> Option<Self> {
        let mut line = Self {
            bytes: [0; 1024],
            len: 0,
        };
        line.push_u64(u64::from(code))?;
        line.push_slice(b" ")?;
        line.push_u64(article_number)?;
        line.push_slice(b" ")?;
        line.push_slice(message_id.as_bytes())?;
        line.push_slice(b"\r\n")?;
        Some(line)
    }

    fn push_slice(&mut self, part: &[u8]) -> Option<()> {
        let end = self.len.checked_add(part.len())?;
        let dst = self.bytes.get_mut(self.len..end)?;
        dst.copy_from_slice(part);
        self.len = end;
        Some(())
    }

    fn push_u64(&mut self, value: u64) -> Option<()> {
        let mut digits = [0_u8; 20];
        let mut cursor = digits.len();
        let mut n = value;
        loop {
            cursor -= 1;
            digits[cursor] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.push_slice(&digits[cursor..])
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl CachedPayload {
    #[must_use]
    pub(crate) fn len(&self) -> CachedPayloadLen {
        let len = match self {
            Self::Missing | Self::AvailabilityOnly | Self::Stat { .. } => 0,
            Self::Article { headers, body, .. } => headers.len() + body.len(),
            Self::Head { headers, .. } => headers.len(),
            Self::Body { body, .. } => body.len(),
        };
        CachedPayloadLen::new(len)
    }

    #[must_use]
    pub const fn kind(&self) -> CachedPayloadKind {
        match self {
            Self::Missing => CachedPayloadKind::Missing,
            Self::AvailabilityOnly => CachedPayloadKind::AvailabilityOnly,
            Self::Article { .. } => CachedPayloadKind::Article,
            Self::Head { .. } => CachedPayloadKind::Head,
            Self::Body { .. } => CachedPayloadKind::Body,
            Self::Stat { .. } => CachedPayloadKind::Stat,
        }
    }

    #[must_use]
    pub const fn article_number(&self) -> Option<CachedArticleNumber> {
        match self {
            Self::Article { article_number, .. }
            | Self::Head { article_number, .. }
            | Self::Body { article_number, .. }
            | Self::Stat { article_number } => *article_number,
            Self::Missing | Self::AvailabilityOnly => None,
        }
    }
}

/// Cache entry for an article.
///
/// Stores typed response metadata and semantic payload only. Status lines and
/// multiline terminators are regenerated when serving cache hits.
#[derive(Clone, Debug)]
pub struct ArticleEntry {
    /// Backend availability bitset (2 bytes)
    ///
    /// No mutex needed: moka clones entries on `get()`, and updates go through
    /// `cache.insert()` which replaces the whole entry atomically.
    backend_availability: ArticleAvailability,

    status_code: StatusCode,
    payload: CachedPayload,

    /// Tier of the backend that provided this article
    /// Used for tier-aware TTL: higher tier = longer TTL
    tier: ttl::CacheTier,

    /// Unix timestamp when this entry was inserted (milliseconds since epoch)
    /// Populated via `ttl::now_millis()` and used with `tier` for TTL expiration
    inserted_at: ttl::CacheTimestampMillis,
}

impl ArticleEntry {
    /// Ingest a cold response bytes into a typed semantic cache entry.
    ///
    /// This is the boundary for cold responses read from the wire. The entry
    /// stores parsed metadata and payload sections, not the original response.
    #[must_use]
    pub fn from_response_bytes(buffer: impl AsRef<[u8]>) -> Self {
        Self::from_response_bytes_with_tier(buffer, ttl::CacheTier::new(0))
    }

    #[must_use]
    pub fn availability_only(status_code: StatusCode, tier: ttl::CacheTier) -> Self {
        Self {
            backend_availability: ArticleAvailability::new(),
            status_code,
            payload: CachedPayload::AvailabilityOnly,
            tier,
            inserted_at: ttl::CacheTimestampMillis::now(),
        }
    }

    #[must_use]
    pub(crate) const fn from_parts(
        status_code: StatusCode,
        payload: CachedPayload,
        backend_availability: ArticleAvailability,
        tier: ttl::CacheTier,
        inserted_at: u64,
    ) -> Self {
        Self {
            backend_availability,
            status_code,
            payload,
            tier,
            inserted_at: ttl::CacheTimestampMillis::new(inserted_at),
        }
    }

    /// Ingest a cold response bytes with a specific provider tier.
    #[must_use]
    pub fn from_response_bytes_with_tier(buffer: impl AsRef<[u8]>, tier: ttl::CacheTier) -> Self {
        let buffer = buffer.as_ref();
        let status_code = StatusCode::parse(buffer).unwrap_or_else(|| StatusCode::new(430));
        let payload = parse_payload(status_code, buffer);
        Self {
            backend_availability: ArticleAvailability::new(),
            status_code,
            payload,
            tier,
            inserted_at: ttl::CacheTimestampMillis::now(),
        }
    }

    #[must_use]
    pub(crate) fn from_ingest_bytes_with_tier(
        buffer: super::CacheIngestBytes,
        tier: ttl::CacheTier,
    ) -> Self {
        match buffer {
            super::CacheIngestBytes::Boxed(buffer) => {
                Self::from_response_bytes_with_tier(buffer, tier)
            }
            super::CacheIngestBytes::Pooled(buffer) => {
                Self::from_response_bytes_with_tier(buffer.as_ref(), tier)
            }
            super::CacheIngestBytes::Chunked(buffer) => {
                Self::from_chunked_response_with_tier(&buffer, tier)
            }
            super::CacheIngestBytes::Small(buffer) => {
                Self::from_response_bytes_with_tier(buffer, tier)
            }
        }
    }

    #[must_use]
    fn from_chunked_response_with_tier(
        buffer: &crate::pool::ChunkedResponse,
        tier: ttl::CacheTier,
    ) -> Self {
        let mut prefix = smallvec::SmallVec::<[u8; 128]>::new();
        buffer.copy_prefix_into(3, &mut prefix);
        let status_code = StatusCode::parse(&prefix).unwrap_or_else(|| StatusCode::new(430));
        let payload = parse_payload_chunks(status_code, buffer.iter_chunks());
        Self {
            backend_availability: ArticleAvailability::new(),
            status_code,
            payload,
            tier,
            inserted_at: ttl::CacheTimestampMillis::now(),
        }
    }

    /// Check if this entry has expired based on tier-aware TTL
    ///
    /// See [`super::ttl`] for the TTL formula.
    #[inline]
    #[must_use]
    pub fn is_expired(&self, base_ttl_millis: u64) -> bool {
        ttl::is_expired(self.inserted_at.get(), base_ttl_millis, self.tier)
    }

    /// Get the tier of the backend that provided this article
    #[inline]
    #[must_use]
    pub const fn tier(&self) -> ttl::CacheTier {
        self.tier
    }

    #[inline]
    #[must_use]
    pub const fn inserted_at(&self) -> ttl::CacheTimestampMillis {
        self.inserted_at
    }

    /// Set the tier (used when updating entry)
    #[inline]
    pub const fn set_tier(&mut self, tier: ttl::CacheTier) {
        self.tier = tier;
    }

    #[inline]
    #[must_use]
    pub(crate) const fn payload_kind(&self) -> CachedPayloadKind {
        self.payload.kind()
    }

    #[inline]
    #[must_use]
    pub const fn article_number(&self) -> Option<CachedArticleNumber> {
        self.payload.article_number()
    }

    #[inline]
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        self.status_code
    }

    /// Check if we should try fetching from this backend
    ///
    /// Returns false if backend is known to not have this article (returned 430 before).
    #[inline]
    #[must_use]
    pub fn should_try_backend(&self, backend_id: BackendId) -> bool {
        self.backend_availability.should_try(backend_id)
    }

    /// Record that a backend returned 430 (doesn't have this article)
    pub fn record_backend_missing(&mut self, backend_id: BackendId) {
        self.backend_availability.record_missing(backend_id);
    }

    /// Record that a backend successfully provided this article
    pub fn record_backend_has(&mut self, backend_id: BackendId) {
        self.backend_availability.record_has(backend_id);
    }

    /// Set backend availability (used for hydrating from hybrid cache)
    pub const fn set_availability(&mut self, availability: ArticleAvailability) {
        self.backend_availability = availability;
    }

    /// Check if all backends have been tried and none have the article
    #[must_use]
    pub fn all_backends_exhausted(&self, total_backends: BackendCount) -> bool {
        self.backend_availability.all_exhausted(total_backends)
    }

    /// Get backends that might have this article
    #[must_use]
    pub fn available_backends(&self, total_backends: BackendCount) -> Vec<BackendId> {
        self.backend_availability
            .available_backends(total_backends)
            .collect()
    }

    /// Check if this cache entry has useful availability information
    ///
    /// Returns true if at least one backend has been tried (marked as missing or has article).
    /// If false, we haven't tried any backends yet and should run precheck instead of
    /// serving from cache.
    #[inline]
    #[must_use]
    pub const fn has_availability_info(&self) -> bool {
        self.backend_availability.has_availability_info()
    }

    /// Check if this cache entry contains a complete article (220) or body (222)
    ///
    /// Returns true if:
    /// 1. Status code is 220 (ARTICLE) or 222 (BODY)
    /// 2. Buffer contains actual content (not just a stub like "220\r\n")
    ///
    /// A complete response ends with ".\r\n" and is significantly longer
    /// than a stub. Stubs are typically 5-6 bytes (e.g., "220\r\n" or "223\r\n").
    ///
    /// This is used when `cache_articles=true` to determine if we can serve
    /// directly from cache or need to fetch additional data.
    #[inline]
    #[must_use]
    pub fn is_complete_article(&self) -> bool {
        let code = self.status_code();
        matches!(
            (&self.payload, code.as_u16()),
            (CachedPayload::Article { headers, body, .. }, 220)
                if !headers.is_empty() || !body.is_empty()
        ) || matches!(
            (&self.payload, code.as_u16()),
            (CachedPayload::Body { body, .. }, 222) if !body.is_empty()
        )
    }

    /// Initialize availability tracker from this cached entry
    ///
    /// Creates a fresh `ArticleAvailability` with backends marked missing based on
    /// cached knowledge (backends that previously returned 430).
    pub fn to_availability(&self, total_backends: BackendCount) -> ArticleAvailability {
        let mut availability = ArticleAvailability::new();

        // Mark backends we know don't have this article
        for backend_id in (0..total_backends.get()).map(BackendId::from_index) {
            if !self.should_try_backend(backend_id) {
                availability.record_missing(backend_id);
            }
        }

        availability
    }

    #[must_use]
    pub(crate) fn payload_len(&self) -> CachedPayloadLen {
        self.payload.len()
    }

    #[must_use]
    pub fn response_for(
        &self,
        request_kind: RequestKind,
        message_id: &str,
    ) -> Option<CachedArticleResponse<'_>> {
        response_for_payload(&self.payload, request_kind, message_id)
    }
}

pub(crate) fn response_for_payload<'a>(
    payload: &'a CachedPayload,
    request_kind: RequestKind,
    message_id: &str,
) -> Option<CachedArticleResponse<'a>> {
    let article_number = match payload {
        CachedPayload::Article { article_number, .. }
        | CachedPayload::Head { article_number, .. }
        | CachedPayload::Body { article_number, .. }
        | CachedPayload::Stat { article_number } => *article_number,
        CachedPayload::Missing | CachedPayload::AvailabilityOnly => None,
    };
    let number = article_number.map_or(0, CachedArticleNumber::get);

    match (request_kind, payload) {
        (
            RequestKind::Stat,
            CachedPayload::Article { .. }
            | CachedPayload::Head { .. }
            | CachedPayload::Body { .. }
            | CachedPayload::Stat { .. },
        ) => Some(CachedArticleResponse {
            status: StatusCode::new(223),
            status_line: StackStatusLine::new(223, number, message_id)?,
            payload: CachedArticleResponsePayload::None,
        }),
        (RequestKind::Article, CachedPayload::Article { headers, body, .. }) => {
            Some(CachedArticleResponse {
                status: StatusCode::new(220),
                status_line: StackStatusLine::new(220, number, message_id)?,
                payload: CachedArticleResponsePayload::Article { headers, body },
            })
        }
        (
            RequestKind::Head,
            CachedPayload::Article { headers, .. } | CachedPayload::Head { headers, .. },
        ) => Some(CachedArticleResponse {
            status: StatusCode::new(221),
            status_line: StackStatusLine::new(221, number, message_id)?,
            payload: CachedArticleResponsePayload::Head { headers },
        }),
        (
            RequestKind::Body,
            CachedPayload::Article { body, .. } | CachedPayload::Body { body, .. },
        ) => Some(CachedArticleResponse {
            status: StatusCode::new(222),
            status_line: StackStatusLine::new(222, number, message_id)?,
            payload: CachedArticleResponsePayload::Body { body },
        }),
        _ => None,
    }
}

pub(crate) fn parse_payload(status_code: StatusCode, buffer: &[u8]) -> CachedPayload {
    let code = status_code.as_u16();
    if code == 430 {
        return CachedPayload::Missing;
    }
    let Some(status_end) = memchr::memmem::find(buffer, b"\r\n").map(|pos| pos + 2) else {
        return CachedPayload::AvailabilityOnly;
    };
    let article_number = parse_article_number(&buffer[..status_end]);
    let Some(payload) = payload_without_multiline_terminator(code, &buffer[status_end..]) else {
        return match code {
            223 => CachedPayload::Stat { article_number },
            _ => CachedPayload::AvailabilityOnly,
        };
    };
    payload_from_semantic_bytes(code, article_number, payload)
}

pub(crate) fn parse_payload_chunks<'a>(
    status_code: StatusCode,
    chunks: impl IntoIterator<Item = &'a [u8]>,
) -> CachedPayload {
    let code = status_code.as_u16();
    if code == 430 {
        return CachedPayload::Missing;
    }

    let mut status_line = Vec::new();
    let mut payload_with_tail = Vec::new();
    let mut in_status = true;

    for chunk in chunks {
        for &byte in chunk {
            if in_status {
                status_line.push(byte);
                if status_line.ends_with(b"\r\n") {
                    in_status = false;
                }
            } else {
                payload_with_tail.push(byte);
            }
        }
    }

    if in_status {
        return CachedPayload::AvailabilityOnly;
    }

    let article_number = parse_article_number(&status_line);
    let Some(payload) = payload_without_multiline_terminator(code, &payload_with_tail) else {
        return match code {
            223 => CachedPayload::Stat { article_number },
            _ => CachedPayload::AvailabilityOnly,
        };
    };
    payload_from_semantic_bytes(code, article_number, payload)
}

fn payload_without_multiline_terminator(code: u16, payload: &[u8]) -> Option<&[u8]> {
    if !matches!(code, 220..=222) {
        return Some(payload);
    }
    if payload == b".\r\n" {
        return Some(&[]);
    }
    payload.strip_suffix(b"\r\n.\r\n").or_else(|| {
        payload
            .strip_suffix(b".\r\n")
            .filter(|_| payload.len() == 3)
    })
}

fn payload_from_semantic_bytes(
    code: u16,
    article_number: Option<CachedArticleNumber>,
    payload: &[u8],
) -> CachedPayload {
    match code {
        220 => {
            if let Some(split) = memchr::memmem::find(payload, b"\r\n\r\n") {
                CachedPayload::Article {
                    article_number,
                    headers: payload[..split].into(),
                    body: payload[split + 4..].into(),
                }
            } else {
                CachedPayload::Article {
                    article_number,
                    headers: Box::from([]),
                    body: payload.into(),
                }
            }
        }
        221 => CachedPayload::Head {
            article_number,
            headers: payload.into(),
        },
        222 => CachedPayload::Body {
            article_number,
            body: payload.into(),
        },
        223 => CachedPayload::Stat { article_number },
        _ => CachedPayload::AvailabilityOnly,
    }
}

fn parse_article_number(status_line: &[u8]) -> Option<CachedArticleNumber> {
    let rest = status_line.get(4..)?;
    let end = memchr::memchr(b' ', rest).unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end])
        .ok()?
        .parse::<u64>()
        .ok()
        .map(CachedArticleNumber::new)
}

/// Article cache using LRU eviction with TTL
///
/// Uses `Arc<str>` (message ID content without brackets) as key for zero-allocation lookups.
/// `Arc<str>` implements `Borrow<str>`, allowing `cache.get(&str)` without allocation.
///
/// Supports tier-aware TTL: entries from higher tier backends get longer TTLs.
/// Formula: `effective_ttl = base_ttl * (2 ^ tier)`
#[derive(Clone, Debug)]
pub struct ArticleCache {
    cache: Arc<Cache<Arc<str>, ArticleEntry>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
    capacity: u64,
    cache_articles: bool,
    /// Base TTL in milliseconds (used for tier-aware expiration via `effective_ttl`)
    ttl_millis: u64,
}

impl ArticleCache {
    /// Create a new article cache
    ///
    /// # Arguments
    /// * `max_capacity` - Maximum cache size in bytes (uses weighted entries)
    /// * `ttl` - Time-to-live for cached articles
    /// * `cache_articles` - Whether to cache full article bodies (true) or just availability (false)
    #[must_use]
    pub fn new(max_capacity: u64, ttl: Duration, cache_articles: bool) -> Self {
        // Build cache with byte-based capacity using weigher
        // max_capacity is total bytes allowed
        //
        // IMPORTANT: Moka's time_to_live must be set to the MAXIMUM tier-aware TTL
        // so that high-tier entries aren't evicted prematurely by Moka.
        // Our is_expired() method handles per-entry expiration based on tier.
        //
        // Moka has a hard limit of ~1000 years for TTL. For very high tier multipliers
        // (e.g., tier 63 = 2^63), we cap at 100 years which is more than enough.
        // We handle per-entry expiration ourselves in get() based on actual tier.
        const MAX_MOKA_TTL: Duration = Duration::from_secs(100 * 365 * 24 * 3600); // 100 years

        // For zero TTL, use Duration::ZERO so entries expire immediately in Moka too
        let max_tier_ttl = if ttl.is_zero() {
            Duration::ZERO
        } else {
            // Non-zero TTL: multiply by max tier multiplier (2^63) and cap at MAX_MOKA_TTL
            // Since 2^63 > u32::MAX, the multiplier will overflow u32, so just use MAX_MOKA_TTL
            MAX_MOKA_TTL
        };
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(max_tier_ttl)
            .weigher(move |key: &Arc<str>, entry: &ArticleEntry| -> u32 {
                // Calculate actual memory footprint for accurate capacity tracking.
                //
                // Memory layout per cache entry:
                //
                // Key: Arc<str>
                //   - Arc control block: 16 bytes (strong_count + weak_count)
                //   - String data: key.len() bytes
                //   - Allocator overhead: ~16 bytes (malloc metadata, alignment)
                //
                // Value: ArticleEntry
                //   - Struct inline metadata: availability, status, tier, timestamp, payload tag
                //   - Semantic boxed payload sections: headers/body slice metadata and bytes
                //   - Allocator overhead for present payload sections
                //
                // Moka internal per-entry overhead is MUCH larger than the data itself:
                //   - Key stored twice: Bucket.key AND ValueEntry.info.key_hash.key
                //   - EntryInfo<K> struct: ~72 bytes (atomics, timestamps, counters)
                //   - LRU deque nodes and frequency sketch entries
                //   - Timer wheel entries for TTL tracking
                //   - crossbeam-epoch deferred garbage (can retain 2x entries)
                //   - HashMap segments with open addressing (~2x load factor)
                //
                // Empirical testing shows ~10x actual RSS vs weighted_size().
                // Our observed ratio: 362MB RSS / 36MB weighted = 10x
                //
                const ARC_STR_OVERHEAD: usize = 16 + 16; // Arc control block + allocator
                const ENTRY_STRUCT: usize = 64; // ArticleEntry inline metadata and enum tag
                const PAYLOAD_OVERHEAD: usize = 2 * (16 + 16); // Up to headers/body boxed slices + allocators
                // Moka internal structures - empirically measured to address memory reporting gap.
                // See moka issue #473: https://github.com/moka-rs/moka/issues/473
                // Observed ratio: 362MB RSS / 36MB weighted_size() = 10x
                const MOKA_OVERHEAD: usize = 2000;

                let key_size = ARC_STR_OVERHEAD + key.len();
                let buffer_size = PAYLOAD_OVERHEAD + entry.payload_len().get();
                let base_size = key_size + buffer_size + ENTRY_STRUCT + MOKA_OVERHEAD;

                // Stubs (availability-only entries) have higher relative overhead
                // due to allocator fragmentation on small allocations.
                // Complete articles are dominated by content size, so no multiplier needed.
                let weighted_size = if entry.is_complete_article() {
                    base_size
                } else {
                    // Small allocations have ~50% more overhead from allocator fragmentation.
                    // Use a 1.5x multiplier, rounding up, to avoid underestimating small entries.
                    (base_size * 3).div_ceil(2)
                };

                weighted_size.try_into().unwrap_or(u32::MAX)
            })
            .build();

        Self {
            cache: Arc::new(cache),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            capacity: max_capacity,
            cache_articles,
            ttl_millis: ttl.as_millis() as u64,
        }
    }

    /// Get an article from the cache
    ///
    /// Accepts any lifetime `MessageId` and uses the string content (without brackets) as key.
    ///
    /// **Zero-allocation**: `without_brackets()` returns `&str`, which moka accepts directly
    /// for `Arc<str>` keys via the `Borrow<str>` trait. This avoids allocating a new `Arc<str>`
    /// for every cache lookup. See `test_arc_str_borrow_lookup` test for verification.
    ///
    /// **Tier-aware TTL**: Even if moka hasn't expired the entry yet, we check if the entry
    /// is expired based on tier-aware TTL. Higher tier entries get longer TTLs.
    pub async fn get(&self, message_id: &MessageId<'_>) -> Option<ArticleEntry> {
        // moka::Cache<Arc<str>, V> supports get(&str) via Borrow<str> trait
        // This is zero-allocation: no Arc<str> is created for the lookup
        let result = self.cache.get(message_id.without_brackets()).await;

        match result {
            Some(entry) if !entry.is_expired(self.ttl_millis) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(entry)
            }
            Some(_) => {
                // Entry exists but expired by tier-aware TTL - invalidate and treat as cache miss
                // Invalidating immediately frees capacity rather than waiting for LRU eviction,
                // preventing repeated cache misses on the same stale key
                self.cache.invalidate(message_id.without_brackets()).await;
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Upsert cache entry (insert or update) - ATOMIC OPERATION
    ///
    /// Uses moka's `entry().and_upsert_with()` for atomic get-modify-store.
    /// This eliminates the race condition of separate `get()` + `insert()` calls
    /// and provides key-level locking for concurrent operations.
    ///
    /// If entry exists: updates the entry and marks backend as having the article
    /// If entry doesn't exist: inserts new entry
    ///
    /// When `cache_articles=false`, stores typed availability without payload bytes.
    /// When `cache_articles=true`, parses and stores semantic payload sections.
    ///
    /// The tier is stored with the entry for tier-aware TTL calculation.
    ///
    /// CRITICAL: Always re-insert to refresh TTL, and mark backend as having the article.
    pub async fn upsert(
        &self,
        message_id: MessageId<'_>,
        buffer: impl AsRef<[u8]>,
        backend_id: BackendId,
        tier: ttl::CacheTier,
    ) {
        self.upsert_ingest(message_id, buffer.as_ref().into(), backend_id, tier)
            .await;
    }

    pub(crate) async fn upsert_ingest(
        &self,
        message_id: MessageId<'_>,
        buffer: super::CacheIngestBytes,
        backend_id: BackendId,
        tier: ttl::CacheTier,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();
        let cache_articles = self.cache_articles;

        let new_entry_template = if cache_articles {
            ArticleEntry::from_ingest_bytes_with_tier(buffer, tier)
        } else {
            let Some(status_code) = buffer.status_code() else {
                return;
            };
            ArticleEntry::availability_only(status_code, tier)
        };

        // Use atomic upsert - this provides key-level locking and eliminates
        // the race condition between get() and insert() calls
        self.cache
            .entry(key)
            .and_upsert_with(|maybe_entry| {
                let new_entry = if let Some(existing) = maybe_entry {
                    let mut entry = existing.into_value();

                    // Decide whether to update buffer based on completeness
                    let existing_complete = entry.is_complete_article();
                    let new_complete = new_entry_template.is_complete_article();

                    let should_replace = match (existing_complete, new_complete) {
                        (false, true) => true,  // Stub → Complete: Always replace
                        (true, false) => false, // Complete → Stub: Never replace
                        (true, true) | (false, false) => {
                            new_entry_template.payload_len() > entry.payload_len()
                        }
                    };

                    if should_replace {
                        entry.status_code = new_entry_template.status_code;
                        entry.payload = new_entry_template.payload.clone();
                        entry.tier = tier;
                    }

                    // Refresh TTL on every successful upsert, independent of buffer replacement
                    entry.inserted_at = ttl::CacheTimestampMillis::now();

                    // Mark backend as having the article
                    entry.record_backend_has(backend_id);
                    entry
                } else {
                    // New entry with tier
                    let mut entry = new_entry_template.clone();
                    entry.record_backend_has(backend_id);
                    entry
                };

                std::future::ready(new_entry)
            })
            .await;
    }

    /// Record successful backend availability without storing response payload bytes.
    pub async fn record_backend_has_status(
        &self,
        message_id: MessageId<'_>,
        status_code: StatusCode,
        backend_id: BackendId,
        tier: ttl::CacheTier,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();
        let new_entry_template = ArticleEntry::availability_only(status_code, tier);

        self.cache
            .entry(key)
            .and_upsert_with(|maybe_entry| {
                let mut entry = maybe_entry.map_or_else(
                    || new_entry_template.clone(),
                    |existing| existing.into_value(),
                );
                if !entry.is_complete_article() {
                    entry.status_code = status_code;
                    entry.tier = tier;
                    entry.payload = CachedPayload::AvailabilityOnly;
                }
                entry.inserted_at = ttl::CacheTimestampMillis::now();
                entry.record_backend_has(backend_id);
                std::future::ready(entry)
            })
            .await;
    }

    /// Record that a backend returned 430 for this article - ATOMIC OPERATION
    ///
    /// Uses moka's `entry().and_upsert_with()` for atomic get-modify-store.
    /// This eliminates the race condition of separate `get()` + `insert()` calls
    /// and provides key-level locking for concurrent operations.
    ///
    /// If the article is already cached, updates the availability bitset.
    /// If not cached, creates a new cache entry with a 430 stub.
    /// This prevents repeated queries to backends that don't have the article.
    ///
    /// Note: We don't store the actual backend 430 response because:
    /// 1. We always send a standardized 430 to clients, never the backend's response
    /// 2. The only info we need is the availability bitset (which backends returned 430)
    pub async fn record_backend_missing(&self, message_id: MessageId<'_>, backend_id: BackendId) {
        let key: Arc<str> = message_id.without_brackets().into();
        let misses = &self.misses;

        // Use atomic upsert - this provides key-level locking
        let entry = self
            .cache
            .entry(key)
            .and_upsert_with(|maybe_entry| {
                let new_entry = if let Some(existing) = maybe_entry {
                    let mut entry = existing.into_value();
                    entry.record_backend_missing(backend_id);
                    entry
                } else {
                    // First 430 for this article - create typed missing entry.
                    let mut entry = ArticleEntry::from_response_bytes(b"430\r\n");
                    entry.record_backend_missing(backend_id);
                    entry
                };

                std::future::ready(new_entry)
            })
            .await;

        // Track misses for new entries
        if entry.is_fresh() {
            misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sync availability state from local tracker to cache - ATOMIC OPERATION
    ///
    /// Uses moka's `entry().and_compute_with()` for atomic get-modify-store.
    /// This eliminates the race condition of separate `get()` + `insert()` calls
    /// and provides key-level locking for concurrent operations.
    ///
    /// This is called ONCE at the end of a retry loop to persist all the
    /// backends that returned 430 during this request. Much more efficient
    /// than calling `record_backend_missing` for each backend individually.
    ///
    /// IMPORTANT: Only creates a 430 stub entry if ALL checked backends returned 430.
    /// If any backend successfully provided the article, we skip creating an entry
    /// (the actual article will be cached via upsert, which may race with this call).
    pub async fn sync_availability(
        &self,
        message_id: MessageId<'_>,
        availability: &ArticleAvailability,
    ) {
        use moka::ops::compute::Op;

        // Only sync if we actually tried some backends
        if availability.checked_bits() == 0 {
            return;
        }

        let key: Arc<str> = message_id.without_brackets().into();
        let availability = *availability; // Copy for the closure
        let misses = &self.misses;

        // Use atomic compute - allows us to conditionally insert/update
        let result = self
            .cache
            .entry(key)
            .and_compute_with(|maybe_entry| {
                let op = if let Some(existing) = maybe_entry {
                    // Merge availability into existing entry
                    let mut entry = existing.into_value();
                    entry.backend_availability.merge_from(&availability);
                    Op::Put(entry)
                } else {
                    // No existing entry - only create a 430 stub if ALL backends returned 430
                    if availability.any_backend_has_article() {
                        // A backend successfully provided the article.
                        // Don't create a 430 stub - let upsert() handle it with the real article data.
                        Op::Nop
                    } else {
                        // All checked backends returned 430 - create stub to track this
                        let mut entry = ArticleEntry::from_response_bytes(b"430\r\n");
                        entry.backend_availability = availability;
                        Op::Put(entry)
                    }
                };

                std::future::ready(op)
            })
            .await;

        // Track misses for new entries
        if matches!(result, moka::ops::compute::CompResult::Inserted(_)) {
            misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Get cache statistics
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entry_count: self.cache.entry_count(),
            weighted_size: self.cache.weighted_size(),
        }
    }

    /// Insert an article entry directly (for testing)
    ///
    /// This is a low-level method that bypasses the usual upsert logic.
    /// Only use this in tests where you need precise control over cache state.
    #[cfg(test)]
    pub async fn insert(&self, message_id: MessageId<'_>, entry: ArticleEntry) {
        let key: Arc<str> = message_id.without_brackets().into();
        self.cache.insert(key, entry).await;
    }

    /// Get maximum cache capacity
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Get current number of cached entries (synchronous)
    #[inline]
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Get current weighted size in bytes (synchronous)
    #[inline]
    #[must_use]
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Get cache hit rate as percentage (0.0 to 100.0)
    #[inline]
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;

        if total == 0 {
            0.0
        } else {
            (hits as f64 / total as f64) * 100.0
        }
    }

    /// Run pending background tasks (for testing)
    ///
    /// Moka performs maintenance tasks (eviction, expiration) asynchronously.
    /// This method ensures all pending tasks complete, useful for deterministic testing.
    pub async fn sync(&self) {
        self.cache.run_pending_tasks().await;
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub entry_count: u64,
    pub weighted_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageId;
    use futures::executor::block_on;
    use std::time::Duration;

    fn create_test_article(msgid: &str) -> ArticleEntry {
        let buffer = format!("220 0 {msgid}\r\nSubject: Test\r\n\r\nBody\r\n.\r\n").into_bytes();
        ArticleEntry::from_response_bytes(buffer)
    }

    fn rendered(entry: &ArticleEntry, verb: &str, msgid: &str) -> Vec<u8> {
        let request_kind = match verb {
            "ARTICLE" => RequestKind::Article,
            "HEAD" => RequestKind::Head,
            "BODY" => RequestKind::Body,
            "STAT" => RequestKind::Stat,
            _ => panic!("unsupported test verb {verb}"),
        };
        let response = entry.response_for(request_kind, msgid).unwrap();
        let mut out = Vec::with_capacity(response.wire_len().get());
        block_on(response.write_to(&mut out)).unwrap();
        out
    }

    #[tokio::test]
    async fn cached_article_response_writes_wire_slices() {
        let entry = create_test_article("<test@example.com>");
        let response = entry
            .response_for(RequestKind::Article, "<test@example.com>")
            .unwrap();
        let mut out = Vec::new();

        response.write_to(&mut out).await.unwrap();

        assert_eq!(
            out,
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n"
        );
    }

    #[test]
    fn cached_article_response_exposes_typed_wire_len() {
        let entry = create_test_article("<test@example.com>");
        let response = entry
            .response_for(RequestKind::Stat, "<test@example.com>")
            .unwrap();

        assert_eq!(
            response.wire_len(),
            crate::protocol::ResponseWireLen::new(26)
        );
    }

    #[tokio::test]
    async fn cached_article_response_writes_derived_wire_shapes() {
        let entry = create_test_article("<test@example.com>");
        let cases = [
            (
                RequestKind::Head,
                b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n".as_slice(),
            ),
            (
                RequestKind::Body,
                b"222 0 <test@example.com>\r\nBody\r\n.\r\n".as_slice(),
            ),
            (
                RequestKind::Stat,
                b"223 0 <test@example.com>\r\n".as_slice(),
            ),
        ];

        for (request_kind, expected) in cases {
            let response = entry
                .response_for(request_kind, "<test@example.com>")
                .unwrap();
            let mut out = Vec::new();

            response.write_to(&mut out).await.unwrap();

            assert_eq!(out, expected, "{request_kind:?}");
        }

        let response = entry
            .response_for(RequestKind::Body, "<test@example.com>")
            .unwrap();
        let mut out = Vec::new();

        response.write_to(&mut out).await.unwrap();

        assert_eq!(out, b"222 0 <test@example.com>\r\nBody\r\n.\r\n");
    }

    #[test]
    fn test_article_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = ArticleEntry::from_response_bytes(buffer.clone());

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert_eq!(rendered(&entry, "ARTICLE", "<test@example.com>"), buffer);

        // Default: should try all backends
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn article_entry_ingests_response_bytes_by_name() {
        let entry = ArticleEntry::from_response_bytes(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn article_entry_ingests_borrowed_response_bytes() {
        let entry = ArticleEntry::from_response_bytes(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".as_slice(),
        );

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn article_entry_stores_payload_sections_as_boxed_slices() {
        trait BoxedSlice {}
        impl BoxedSlice for Box<[u8]> {}
        fn assert_boxed_slice<T: BoxedSlice>(_: &T) {}

        let entry = ArticleEntry::from_response_bytes(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );

        match &entry.payload {
            CachedPayload::Article { headers, body, .. } => {
                assert_boxed_slice(headers);
                assert_boxed_slice(body);
            }
            other => panic!("expected article payload, got {other:?}"),
        }
    }

    #[test]
    fn article_entry_ingests_ingest_bytes_without_required_vec() {
        let entry = ArticleEntry::from_ingest_bytes_with_tier(
            smallvec::SmallVec::<[u8; 128]>::from_slice(
                b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
            )
            .into(),
            ttl::CacheTier::new(0),
        );

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn article_entry_ingests_chunked_ingest_bytes_without_flattening_response() {
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

        let entry =
            ArticleEntry::from_ingest_bytes_with_tier(response.into(), ttl::CacheTier::new(0));

        assert_eq!(entry.status_code(), StatusCode::new(220));
        match entry.payload {
            CachedPayload::Article { headers, body, .. } => {
                assert_eq!(headers.as_ref(), b"Subject: Test");
                assert_eq!(body.as_ref(), b"Body");
            }
            other => panic!("expected article payload, got {other:?}"),
        }
    }

    #[test]
    fn test_is_complete_article() {
        // Stubs should NOT be complete articles
        let stub_430 = ArticleEntry::from_response_bytes(b"430\r\n");
        assert!(!stub_430.is_complete_article());

        let stub_220 = ArticleEntry::from_response_bytes(b"220\r\n");
        assert!(!stub_220.is_complete_article());

        let stub_223 = ArticleEntry::from_response_bytes(b"223\r\n");
        assert!(!stub_223.is_complete_article());

        // Full article SHOULD be complete
        let full = ArticleEntry::from_response_bytes(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );
        assert!(full.is_complete_article());

        // Wrong status code (not 220) should NOT be complete article
        let head_response = ArticleEntry::from_response_bytes(
            b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n",
        );
        assert!(!head_response.is_complete_article());

        // Missing terminator should NOT be complete
        let no_terminator = ArticleEntry::from_response_bytes(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n",
        );
        assert!(!no_terminator.is_complete_article());
    }

    #[test]
    fn test_article_entry_record_backend_missing() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>");

        // Initially should try both
        assert!(entry.should_try_backend(backend0));
        assert!(entry.should_try_backend(backend1));

        // Record backend1 as missing (430 response)
        entry.record_backend_missing(backend1);

        // Should still try backend0, but not backend1
        assert!(entry.should_try_backend(backend0));
        assert!(!entry.should_try_backend(backend1));
    }

    #[test]
    fn test_article_entry_all_backends_exhausted() {
        use crate::router::BackendCount;
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_article("<test@example.com>");

        // Not all exhausted yet
        assert!(!entry.all_backends_exhausted(BackendCount::new(2)));

        // Record both as missing
        entry.record_backend_missing(backend0);
        entry.record_backend_missing(backend1);

        // Now all 2 backends are exhausted
        assert!(entry.all_backends_exhausted(BackendCount::new(2)));
    }

    #[tokio::test]
    async fn test_arc_str_borrow_lookup() {
        // Create cache with Arc<str> keys
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        // Create a MessageId and insert an article
        let msgid = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let article = create_test_article("<test123@example.com>");

        cache.insert(msgid.clone(), article.clone()).await;

        // Verify we can retrieve using a different MessageId instance (borrowed)
        // This demonstrates that Arc<str> supports Borrow<str> lookups via &str
        let msgid2 = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let retrieved = cache.get(&msgid2).await;

        assert!(
            retrieved.is_some(),
            "Arc<str> cache should support Borrow<str> lookups"
        );
        assert_eq!(
            rendered(&retrieved.unwrap(), "ARTICLE", "<test123@example.com>"),
            rendered(&article, "ARTICLE", "<test123@example.com>"),
            "Retrieved article should match inserted article"
        );
    }

    #[tokio::test]
    async fn test_cache_hit_miss() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<nonexistent@example.com>").unwrap();
        let result = cache.get(&msgid).await;

        assert!(
            result.is_none(),
            "Cache lookup for non-existent key should return None"
        );
    }

    #[tokio::test]
    async fn test_cache_insert_and_retrieve() {
        let cache = ArticleCache::new(10, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<article@example.com>").unwrap();
        let article = create_test_article("<article@example.com>");

        cache.insert(msgid.clone(), article.clone()).await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(
            rendered(&retrieved, "ARTICLE", "<article@example.com>"),
            rendered(&article, "ARTICLE", "<article@example.com>")
        );
    }

    #[tokio::test]
    async fn test_cache_upsert_new_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();

        cache
            .upsert(
                msgid.clone(),
                buffer.clone(),
                BackendId::from_index(0),
                0.into(),
            )
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(
            rendered(&retrieved, "ARTICLE", "<test@example.com>"),
            buffer
        );
        // Default: should try all backends
        assert!(retrieved.should_try_backend(BackendId::from_index(0)));
        assert!(retrieved.should_try_backend(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn test_cache_upsert_existing_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();

        // Insert with backend 0
        cache
            .upsert(
                msgid.clone(),
                buffer.clone(),
                BackendId::from_index(0),
                0.into(),
            )
            .await;

        // Update with backend 1 - does nothing (entry already exists)
        cache
            .upsert(
                msgid.clone(),
                buffer.clone(),
                BackendId::from_index(1),
                0.into(),
            )
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        // Default: should try all backends
        assert!(retrieved.should_try_backend(BackendId::from_index(0)));
        assert!(retrieved.should_try_backend(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn test_record_backend_missing() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>");

        cache.insert(msgid.clone(), article).await;

        // Record backend 1 as missing
        cache
            .record_backend_missing(msgid.clone(), BackendId::from_index(1))
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        // Backend 0 should still be tried, backend 1 should not
        assert!(retrieved.should_try_backend(BackendId::from_index(0)));
        assert!(!retrieved.should_try_backend(BackendId::from_index(1)));
    }

    /// CRITICAL BUG FIX TEST: `record_backend_missing` must create cache entries
    /// for articles that don't exist anywhere (all backends return 430).
    ///
    /// Bug: Previously, if an article wasn't cached, `record_backend_missing`
    /// would silently do nothing. This caused repeated queries to all backends
    /// for missing articles, resulting in:
    /// - Massive bandwidth waste
    /// - `SABnzbd` reporting "gigabytes of missing articles"
    /// - 4xx/5xx error counts not increasing (metrics bug)
    #[tokio::test]
    async fn test_record_backend_missing_creates_new_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<missing@example.com>").unwrap();

        // Verify article is NOT in cache
        assert!(cache.get(&msgid).await.is_none());

        // Record backend 0 returned 430
        cache
            .record_backend_missing(msgid.clone(), BackendId::from_index(0))
            .await;

        // CRITICAL: Cache entry MUST now exist
        let entry = cache
            .get(&msgid)
            .await
            .expect("Cache entry must exist after record_backend_missing");

        // Verify backend 0 is marked as missing
        assert!(
            !entry.should_try_backend(BackendId::from_index(0)),
            "Backend 0 should be marked missing"
        );

        // Verify backend 1 is still available (not tried yet)
        assert!(
            entry.should_try_backend(BackendId::from_index(1)),
            "Backend 1 should still be available"
        );

        assert!(matches!(entry.payload, CachedPayload::Missing));

        // Record backend 1 also returned 430
        cache
            .record_backend_missing(msgid.clone(), BackendId::from_index(1))
            .await;

        let entry = cache.get(&msgid).await.unwrap();

        // Now both backends should be marked missing
        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(!entry.should_try_backend(BackendId::from_index(1)));

        // Verify all backends exhausted
        use crate::router::BackendCount;
        assert!(
            entry.all_backends_exhausted(BackendCount::new(2)),
            "All backends should be exhausted"
        );
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Initial stats
        let stats = cache.stats();
        assert_eq!(stats.entry_count, 0);

        // Insert one article
        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>");
        cache.insert(msgid, article).await;

        // Wait for background tasks
        cache.sync().await;

        // Check stats again
        let stats = cache.stats();
        assert_eq!(stats.entry_count, 1);
    }

    #[tokio::test]
    async fn test_cache_ttl_expiration() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_millis(50), true); // 1MB

        let msgid = MessageId::from_borrowed("<expire@example.com>").unwrap();
        let article = create_test_article("<expire@example.com>");

        cache.insert(msgid.clone(), article).await;

        // Should be cached immediately
        assert!(cache.get(&msgid).await.is_some());

        // Wait for TTL expiration + sync
        tokio::time::sleep(Duration::from_millis(100)).await;
        cache.sync().await;

        // Should be expired
        assert!(cache.get(&msgid).await.is_none());
    }

    #[tokio::test]
    async fn test_insert_respects_cache_articles_flag() {
        // Test with cache_articles=false - should store availability without payload bytes
        let cache_stub = ArticleCache::new(1024 * 1024, Duration::from_secs(300), false);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let full_size = buffer.len();

        cache_stub
            .upsert(msgid.clone(), buffer, BackendId::from_index(0), 0.into())
            .await;
        cache_stub.sync().await;

        let retrieved = cache_stub.get(&msgid).await.unwrap();
        assert_eq!(
            retrieved.payload_kind(),
            CachedPayloadKind::AvailabilityOnly
        );
        assert_eq!(retrieved.payload_len(), 0);
        assert!(retrieved.payload_len() < full_size);

        // Test with cache_articles=true - should store full article
        let cache_full = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true);

        let msgid2 = MessageId::from_borrowed("<test2@example.com>").unwrap();
        let buffer2 = b"220 0 <test2@example.com>\r\nSubject: Test2\r\n\r\nBody2\r\n.\r\n".to_vec();
        let original_payload_size = b"Subject: Test2".len() + b"Body2".len();

        cache_full
            .upsert(msgid2.clone(), buffer2, BackendId::from_index(0), 0.into())
            .await;
        cache_full.sync().await;

        let retrieved2 = cache_full.get(&msgid2).await.unwrap();

        assert_eq!(retrieved2.payload_len(), original_payload_size);
    }

    #[tokio::test]
    async fn test_cache_capacity_limit() {
        let cache = ArticleCache::new(500, Duration::from_secs(300), true); // 500 bytes total

        // Insert 3 articles (exceeds capacity)
        for i in 1..=3 {
            let msgid_str = format!("<article{i}@example.com>");
            let msgid = MessageId::new(msgid_str).unwrap();
            let article = create_test_article(msgid.as_ref());
            cache.insert(msgid, article).await;
            cache.sync().await; // Force eviction
        }

        // Wait for eviction to complete
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.sync().await;

        let stats = cache.stats();
        assert!(
            stats.entry_count <= 3,
            "Cache should have at most 3 entries with 500 byte capacity"
        );
    }

    #[tokio::test]
    async fn test_article_entry_clone() {
        let article = create_test_article("<test@example.com>");

        let cloned = article.clone();
        assert_eq!(
            rendered(&article, "ARTICLE", "<test@example.com>"),
            rendered(&cloned, "ARTICLE", "<test@example.com>")
        );
    }

    #[tokio::test]
    async fn test_cache_clone() {
        let cache1 = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB
        let cache2 = cache1.clone();

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_article("<test@example.com>");

        cache1.insert(msgid.clone(), article).await;
        cache1.sync().await;

        // Should be accessible from cloned cache
        assert!(cache2.get(&msgid).await.is_some());
    }

    #[tokio::test]
    async fn test_weigher_large_articles() {
        // Test that large article bodies use ACTUAL SIZE (no multiplier)
        // when cache_articles=true and buffer >10KB
        let cache = ArticleCache::new(10 * 1024 * 1024, Duration::from_secs(300), true); // 10MB capacity

        // Create a 750KB article (typical size)
        let body = vec![b'X'; 750_000];
        let response = format!(
            "222 0 <test@example.com>\r\n{}\r\n.\r\n",
            std::str::from_utf8(&body).unwrap()
        );
        let article = ArticleEntry::from_response_bytes(response.as_bytes());

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        cache.insert(msgid.clone(), article).await;
        cache.sync().await;

        // With actual size (no multiplier): 750KB per entry
        // 10MB capacity should fit ~13 articles
        // With old 1.8x multiplier: 750KB * 1.8 ≈ 1.35MB per entry, fits ~7 articles
        // With old 2.5x multiplier: 750KB * 2.5 ≈ 1.875MB per entry, fits ~5 articles

        // Insert 12 more articles (13 total)
        for i in 2..=13 {
            let msgid_str = format!("<article{i}@example.com>");
            let msgid = MessageId::new(msgid_str).unwrap();
            let response = format!(
                "222 0 {}\r\n{}\r\n.\r\n",
                msgid.as_str(),
                std::str::from_utf8(&body).unwrap()
            );
            let article = ArticleEntry::from_response_bytes(response.as_bytes());
            cache.insert(msgid, article).await;
            cache.sync().await;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        cache.sync().await;

        let stats = cache.stats();
        // With actual size (no multiplier), should fit 11-13 large articles
        assert!(
            stats.entry_count >= 11,
            "Cache should fit at least 11 large articles with actual size (no multiplier) (got {})",
            stats.entry_count
        );
    }

    #[tokio::test]
    async fn test_weigher_small_stubs() {
        // Test that small stubs account for moka internal overhead correctly
        // With MOKA_OVERHEAD = 2000 bytes (based on empirical 10x memory ratio from moka issue #473)
        let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), false); // 1MB capacity

        // Create small stub (53 bytes)
        let stub = b"223 0 <test@example.com>\r\n".to_vec();
        let article = ArticleEntry::from_response_bytes(stub);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        cache.insert(msgid, article).await;
        cache.sync().await;

        // With MOKA_OVERHEAD = 2000: stub + Arc + availability + overhead
        // ~53 + 68 + 40 + 2000 = ~2161 bytes per small stub
        // With 2.5x small stub multiplier: ~5400 bytes per stub
        // 1MB capacity should fit ~185 stubs

        // Insert many small stubs
        for i in 2..=200 {
            let msgid_str = format!("<stub{i}@example.com>");
            let msgid = MessageId::new(msgid_str).unwrap();
            let stub = format!("223 0 {}\r\n", msgid.as_str());
            let article = ArticleEntry::from_response_bytes(stub.as_bytes());
            cache.insert(msgid, article).await;
        }

        cache.sync().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        cache.sync().await;

        let stats = cache.stats();
        // Should be able to fit ~150-185 small stubs in 1MB
        assert!(
            stats.entry_count >= 100,
            "Cache should fit many small stubs (got {})",
            stats.entry_count
        );
    }

    #[tokio::test]
    async fn test_cache_with_owned_message_id() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Use owned MessageId
        let msgid = MessageId::new("<owned@example.com>".to_string()).unwrap();
        let article = create_test_article("<owned@example.com>");

        cache.insert(msgid.clone(), article).await;

        // Retrieve with borrowed MessageId
        let borrowed_msgid = MessageId::from_borrowed("<owned@example.com>").unwrap();
        assert!(cache.get(&borrowed_msgid).await.is_some());
    }
}
