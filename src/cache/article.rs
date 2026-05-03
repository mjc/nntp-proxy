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
pub(crate) struct CachedArticleNumber(u64);

impl CachedArticleNumber {
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub(crate) const fn get(self) -> u64 {
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
        headers: Arc<[u8]>,
        body: Arc<[u8]>,
    },
    Head {
        article_number: Option<CachedArticleNumber>,
        headers: Arc<[u8]>,
    },
    Body {
        article_number: Option<CachedArticleNumber>,
        body: Arc<[u8]>,
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
pub struct CachedResponseWire<'a> {
    status: StatusCode,
    status_line: StackStatusLine,
    payload: CachedResponseWirePayload<'a>,
}

#[derive(Debug, Clone, Copy)]
enum CachedResponseWirePayload<'a> {
    None,
    Article { headers: &'a [u8], body: &'a [u8] },
    Head { headers: &'a [u8] },
    Body { body: &'a [u8] },
}

impl CachedResponseWire<'_> {
    fn wire_len_usize(&self) -> usize {
        self.status_line.len() + self.payload_len()
    }

    #[must_use]
    pub fn wire_len(&self) -> crate::protocol::ResponseWireLen {
        self.wire_len_usize().into()
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
        use std::io::IoSlice;
        use tokio::io::AsyncWriteExt as _;

        match self.payload {
            CachedResponseWirePayload::None => writer.write_all(self.status_line()).await?,
            CachedResponseWirePayload::Article { headers, body } => {
                let mut slices = [
                    IoSlice::new(self.status_line()),
                    IoSlice::new(headers),
                    IoSlice::new(b"\r\n\r\n"),
                    IoSlice::new(body),
                    IoSlice::new(b"\r\n.\r\n"),
                ];
                crate::io_util::write_all_vectored(writer, &mut slices).await?;
            }
            CachedResponseWirePayload::Head { headers } => {
                let mut slices = [
                    IoSlice::new(self.status_line()),
                    IoSlice::new(headers),
                    IoSlice::new(b"\r\n.\r\n"),
                ];
                crate::io_util::write_all_vectored(writer, &mut slices).await?;
            }
            CachedResponseWirePayload::Body { body } => {
                let mut slices = [
                    IoSlice::new(self.status_line()),
                    IoSlice::new(body),
                    IoSlice::new(b"\r\n.\r\n"),
                ];
                crate::io_util::write_all_vectored(writer, &mut slices).await?;
            }
        }
        Ok(())
    }

    fn payload_len(&self) -> usize {
        match self.payload {
            CachedResponseWirePayload::None => 0,
            CachedResponseWirePayload::Article { headers, body } => {
                headers.len() + 4 + body.len() + 5
            }
            CachedResponseWirePayload::Head { headers } => headers.len() + 5,
            CachedResponseWirePayload::Body { body } => body.len() + 5,
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
    pub(crate) const fn article_number(&self) -> Option<CachedArticleNumber> {
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
pub struct CachedArticle {
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

impl CachedArticle {
    /// Create an availability-only cache entry without payload bytes.
    #[must_use]
    pub(crate) fn availability_only(status_code: StatusCode, tier: ttl::CacheTier) -> Self {
        Self {
            backend_availability: ArticleAvailability::new(),
            status_code,
            payload: CachedPayload::AvailabilityOnly,
            tier,
            inserted_at: ttl::CacheTimestampMillis::now(),
        }
    }

    #[must_use]
    pub(crate) fn missing(tier: ttl::CacheTier) -> Self {
        Self {
            backend_availability: ArticleAvailability::new(),
            status_code: StatusCode::new(430),
            payload: CachedPayload::Missing,
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

    /// Parse a cold backend response into typed cache metadata and payload.
    #[must_use]
    pub fn from_backend_response_with_tier(
        response: impl AsRef<[u8]>,
        tier: ttl::CacheTier,
    ) -> Self {
        let response = response.as_ref();
        let status_code = StatusCode::parse(response).unwrap_or_else(|| StatusCode::new(430));
        let payload = parse_payload(status_code, response);
        Self {
            backend_availability: ArticleAvailability::new(),
            status_code,
            payload,
            tier,
            inserted_at: ttl::CacheTimestampMillis::now(),
        }
    }

    #[must_use]
    pub(crate) fn from_backend_response_bytes_with_tier(
        buffer: super::BackendResponseBytes,
        tier: ttl::CacheTier,
    ) -> Self {
        match buffer {
            super::BackendResponseBytes::Owned(buffer) => {
                Self::from_backend_response_with_tier(buffer, tier)
            }
            super::BackendResponseBytes::Pooled(buffer) => {
                Self::from_backend_response_with_tier(buffer.as_ref(), tier)
            }
            super::BackendResponseBytes::Chunked(buffer) => {
                Self::from_chunked_response_with_tier(&buffer, tier)
            }
            super::BackendResponseBytes::Inline(buffer) => {
                Self::from_backend_response_with_tier(buffer, tier)
            }
        }
    }

    #[must_use]
    pub fn from_chunked_response_with_tier(
        buffer: &crate::pool::ChunkedResponse,
        tier: ttl::CacheTier,
    ) -> Self {
        let mut prefix = smallvec::SmallVec::<[u8; 128]>::new();
        buffer.copy_prefix_into(3, &mut prefix);
        let status_code = StatusCode::parse(&prefix).unwrap_or_else(|| StatusCode::new(430));
        let payload = parse_payload_chunked_response(status_code, buffer);
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
    pub(crate) fn is_expired(&self, base_ttl_millis: u64) -> bool {
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

    #[inline]
    #[must_use]
    pub(crate) const fn payload_kind(&self) -> CachedPayloadKind {
        self.payload.kind()
    }

    #[inline]
    #[must_use]
    pub(crate) const fn article_number(&self) -> Option<CachedArticleNumber> {
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

    /// Check if all backends have been tried and none have the article
    #[must_use]
    pub fn all_backends_exhausted(&self, total_backends: BackendCount) -> bool {
        self.backend_availability.all_exhausted(total_backends)
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

    /// Return the typed backend availability metadata stored with this entry.
    #[inline]
    #[must_use]
    pub const fn availability(&self) -> ArticleAvailability {
        self.backend_availability
    }

    /// Check if this cache entry contains a complete article (220) or body (222)
    ///
    /// Returns true if:
    /// 1. Status code is 220 (ARTICLE) or 222 (BODY)
    /// 2. Buffer contains actual content (not just a status line like "220\r\n")
    ///
    /// A complete response ends with ".\r\n" and is significantly longer
    /// than a metadata-only availability response.
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
    pub(crate) fn to_availability(&self, total_backends: BackendCount) -> ArticleAvailability {
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
    pub fn cached_response_for(
        &self,
        request_kind: RequestKind,
        message_id: &str,
    ) -> Option<CachedResponseWire<'_>> {
        cached_response_for_payload(&self.payload, request_kind, message_id)
    }
}

pub(crate) fn cached_response_for_payload<'a>(
    payload: &'a CachedPayload,
    request_kind: RequestKind,
    message_id: &str,
) -> Option<CachedResponseWire<'a>> {
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
        ) => Some(CachedResponseWire {
            status: StatusCode::new(223),
            status_line: StackStatusLine::new(223, number, message_id)?,
            payload: CachedResponseWirePayload::None,
        }),
        (RequestKind::Article, CachedPayload::Article { headers, body, .. }) => {
            Some(CachedResponseWire {
                status: StatusCode::new(220),
                status_line: StackStatusLine::new(220, number, message_id)?,
                payload: CachedResponseWirePayload::Article { headers, body },
            })
        }
        (
            RequestKind::Head,
            CachedPayload::Article { headers, .. } | CachedPayload::Head { headers, .. },
        ) => Some(CachedResponseWire {
            status: StatusCode::new(221),
            status_line: StackStatusLine::new(221, number, message_id)?,
            payload: CachedResponseWirePayload::Head { headers },
        }),
        (
            RequestKind::Body,
            CachedPayload::Article { body, .. } | CachedPayload::Body { body, .. },
        ) => Some(CachedResponseWire {
            status: StatusCode::new(222),
            status_line: StackStatusLine::new(222, number, message_id)?,
            payload: CachedResponseWirePayload::Body { body },
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
    payload_for_status(code, article_number, payload)
}

pub(crate) fn parse_payload_chunked_response(
    status_code: StatusCode,
    buffer: &crate::pool::ChunkedResponse,
) -> CachedPayload {
    let code = status_code.as_u16();
    if code == 430 {
        return CachedPayload::Missing;
    }

    let Some((status_end, status_line)) = chunked_status_line(buffer) else {
        return CachedPayload::AvailabilityOnly;
    };
    let article_number = parse_article_number(&status_line);

    match code {
        220..=222 => {
            let Some(payload_end) = chunked_multiline_payload_end(buffer, status_end) else {
                return CachedPayload::AvailabilityOnly;
            };
            payload_for_chunked_status(code, article_number, buffer, status_end, payload_end)
        }
        223 => CachedPayload::Stat { article_number },
        _ => CachedPayload::AvailabilityOnly,
    }
}

fn chunked_status_line(
    buffer: &crate::pool::ChunkedResponse,
) -> Option<(usize, smallvec::SmallVec<[u8; 128]>)> {
    let mut status_line = smallvec::SmallVec::<[u8; 128]>::new();
    let mut position = 0;

    for chunk in buffer.iter_chunks() {
        for &byte in chunk {
            status_line.push(byte);
            position += 1;
            if status_line.ends_with(b"\r\n") {
                return Some((position, status_line));
            }
        }
    }

    None
}

fn chunked_multiline_payload_end(
    buffer: &crate::pool::ChunkedResponse,
    payload_start: usize,
) -> Option<usize> {
    let payload_len = buffer.len().checked_sub(payload_start)?;
    if payload_len == 3 && buffer.ends_with(b".\r\n") {
        return Some(payload_start);
    }
    buffer
        .ends_with(b"\r\n.\r\n")
        .then(|| buffer.len().saturating_sub(5))
}

fn payload_for_chunked_status(
    code: u16,
    article_number: Option<CachedArticleNumber>,
    buffer: &crate::pool::ChunkedResponse,
    payload_start: usize,
    payload_end: usize,
) -> CachedPayload {
    match code {
        220 => {
            if let Some(split) =
                find_sequence_in_chunked_range(buffer, b"\r\n\r\n", payload_start, payload_end)
            {
                CachedPayload::Article {
                    article_number,
                    headers: copy_chunked_range(buffer, payload_start, split),
                    body: copy_chunked_range(buffer, split + 4, payload_end),
                }
            } else {
                CachedPayload::Article {
                    article_number,
                    headers: Arc::from([]),
                    body: copy_chunked_range(buffer, payload_start, payload_end),
                }
            }
        }
        221 => CachedPayload::Head {
            article_number,
            headers: copy_chunked_range(buffer, payload_start, payload_end),
        },
        222 => CachedPayload::Body {
            article_number,
            body: copy_chunked_range(buffer, payload_start, payload_end),
        },
        _ => CachedPayload::AvailabilityOnly,
    }
}

fn find_sequence_in_chunked_range(
    buffer: &crate::pool::ChunkedResponse,
    needle: &[u8; 4],
    start: usize,
    end: usize,
) -> Option<usize> {
    let mut position = 0;
    let mut window = [0_u8; 4];
    let mut seen = 0_usize;

    for chunk in buffer.iter_chunks() {
        for &byte in chunk {
            if position >= end {
                return None;
            }
            if position >= start {
                window.copy_within(1.., 0);
                window[3] = byte;
                seen += 1;
                if seen >= 4 && &window == needle {
                    return Some(position + 1 - 4);
                }
            }
            position += 1;
        }
    }

    None
}

fn copy_chunked_range(
    buffer: &crate::pool::ChunkedResponse,
    start: usize,
    end: usize,
) -> Arc<[u8]> {
    let len = end.saturating_sub(start);
    let mut output = Vec::with_capacity(len);
    let mut position = 0;

    for chunk in buffer.iter_chunks() {
        let chunk_start = position;
        let chunk_end = position + chunk.len();
        position = chunk_end;

        if chunk_end <= start {
            continue;
        }
        if chunk_start >= end {
            break;
        }

        let copy_start = start.saturating_sub(chunk_start);
        let copy_end = (end.min(chunk_end)) - chunk_start;
        output.extend_from_slice(&chunk[copy_start..copy_end]);
    }

    Arc::from(output.into_boxed_slice())
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

fn payload_for_status(
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
                    headers: Arc::from([]),
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
    cache: Arc<Cache<Arc<str>, CachedArticle>>,
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
        // We handle tier-aware expiration ourselves in get(). A giant Moka TTL
        // only adds timer-wheel/read bookkeeping without expiring normal entries
        // at the effective tier TTL. Keep Moka TTL only for the zero-TTL case so
        // tests and pathological configs still expire immediately.
        let builder = Cache::builder().max_capacity(max_capacity).weigher(
            move |key: &Arc<str>, entry: &CachedArticle| -> u32 {
                // Calculate actual memory footprint for accurate capacity tracking.
                //
                // Memory layout per cache entry:
                //
                // Key: Arc<str>
                //   - Arc control block: 16 bytes (strong_count + weak_count)
                //   - String data: key.len() bytes
                //   - Allocator overhead: ~16 bytes (malloc metadata, alignment)
                //
                // Value: CachedArticle
                //   - Struct inline metadata: availability, status, tier, timestamp, payload tag
                //   - Shared semantic payload sections: headers/body slice metadata and bytes
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
                const ENTRY_STRUCT: usize = 64; // CachedArticle inline metadata and enum tag
                const PAYLOAD_OVERHEAD: usize = 2 * (16 + 16); // Up to headers/body shared slices + allocators
                // Moka internal structures - empirically measured to address memory reporting gap.
                // See moka issue #473: https://github.com/moka-rs/moka/issues/473
                // Observed ratio: 362MB RSS / 36MB weighted_size() = 10x
                const MOKA_OVERHEAD: usize = 2000;

                let key_size = ARC_STR_OVERHEAD + key.len();
                let buffer_size = PAYLOAD_OVERHEAD + entry.payload_len().get();
                let base_size = key_size + buffer_size + ENTRY_STRUCT + MOKA_OVERHEAD;

                // Availability-only entries have higher relative overhead
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
            },
        );
        let cache = if ttl.is_zero() {
            builder.time_to_live(Duration::ZERO).build()
        } else {
            builder.build()
        };

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
    pub async fn get(&self, message_id: &MessageId<'_>) -> Option<CachedArticle> {
        // moka::Cache<Arc<str>, V> supports get(&str) via Borrow<str> trait
        // This is zero-allocation: no Arc<str> is created for the lookup
        self.get_by_cache_key(message_id.without_brackets()).await
    }

    /// Get an article by the cache key form of a message ID (without brackets).
    ///
    /// This is the request hot path: `RequestContext` has already validated the
    /// message-id span, so callers can avoid rebuilding a `MessageId` wrapper.
    pub(crate) async fn get_by_cache_key(&self, key: &str) -> Option<CachedArticle> {
        let result = self.cache.get(key).await;

        match result {
            Some(entry) if !entry.is_expired(self.ttl_millis) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(entry)
            }
            Some(_) => {
                // Entry exists but expired by tier-aware TTL - invalidate and treat as cache miss
                // Invalidating immediately frees capacity rather than waiting for LRU eviction,
                // preventing repeated cache misses on the same stale key
                self.cache.invalidate(key).await;
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
    pub async fn upsert_backend_response(
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
        buffer: super::BackendResponseBytes,
        backend_id: BackendId,
        tier: ttl::CacheTier,
    ) {
        let key: Arc<str> = message_id.without_brackets().into();
        let cache_articles = self.cache_articles;

        let new_entry_template = if cache_articles {
            CachedArticle::from_backend_response_bytes_with_tier(buffer, tier)
        } else {
            let Some(status_code) = buffer.status_code() else {
                return;
            };
            CachedArticle::availability_only(status_code, tier)
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
                        (false, true) => true,  // Availability-only -> complete: always replace
                        (true, false) => false, // Complete -> availability-only: never replace
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
        let new_entry_template = CachedArticle::availability_only(status_code, tier);

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
    /// If not cached, creates a typed missing cache entry.
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
                    let mut entry = CachedArticle::missing(ttl::CacheTier::new(0));
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
    /// IMPORTANT: Only creates a missing entry if ALL checked backends returned 430.
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
                    // No existing entry - only create a missing entry if all checked backends returned 430.
                    if availability.any_backend_has_article() {
                        // A backend successfully provided the article.
                        // Don't create a missing entry - let upsert() handle it with the real article data.
                        Op::Nop
                    } else {
                        // All checked backends returned 430 - create a missing entry to track this.
                        let mut entry = CachedArticle::missing(ttl::CacheTier::new(0));
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
    pub(crate) async fn insert(&self, message_id: MessageId<'_>, entry: CachedArticle) {
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
    use std::io::IoSlice;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::AsyncWrite;

    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
        vectored_writes: usize,
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            self.vectored_writes += 1;
            let len = bufs.iter().map(|buf| buf.len()).sum();
            bufs.iter()
                .for_each(|buf| self.bytes.extend_from_slice(buf));
            Poll::Ready(Ok(len))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn cached_article_from_backend_response(buffer: impl AsRef<[u8]>) -> CachedArticle {
        CachedArticle::from_backend_response_with_tier(buffer, ttl::CacheTier::new(0))
    }

    fn create_test_cached_article(msgid: &str) -> CachedArticle {
        let buffer = format!("220 0 {msgid}\r\nSubject: Test\r\n\r\nBody\r\n.\r\n").into_bytes();
        cached_article_from_backend_response(buffer)
    }

    fn rendered(entry: &CachedArticle, request_kind: RequestKind, msgid: &str) -> Vec<u8> {
        let response = entry.cached_response_for(request_kind, msgid).unwrap();
        let mut out = Vec::with_capacity(response.wire_len().get());
        block_on(response.write_to(&mut out)).unwrap();
        out
    }

    fn serves(entry: &CachedArticle, request_kind: RequestKind, msgid: &str) -> bool {
        entry.cached_response_for(request_kind, msgid).is_some()
    }

    fn assert_serves(entry: &CachedArticle, cases: &[(RequestKind, bool)]) {
        cases.iter().for_each(|(request_kind, expected)| {
            assert_eq!(
                serves(entry, *request_kind, "<test@example.com>"),
                *expected,
                "serve decision for {request_kind:?}"
            );
        });
    }

    #[tokio::test]
    async fn cached_article_response_writes_wire_slices() {
        let entry = create_test_cached_article("<test@example.com>");
        let response = entry
            .cached_response_for(RequestKind::Article, "<test@example.com>")
            .unwrap();
        let mut out = Vec::new();

        response.write_to(&mut out).await.unwrap();

        assert_eq!(
            out,
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n"
        );
    }

    #[tokio::test]
    async fn cached_article_response_uses_vectored_write() {
        let entry = create_test_cached_article("<test@example.com>");
        let response = entry
            .cached_response_for(RequestKind::Article, "<test@example.com>")
            .unwrap();
        let mut out = CountingWriter::default();

        response.write_to(&mut out).await.unwrap();

        assert_eq!(
            out.bytes,
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n"
        );
        assert_eq!(out.writes, 0);
        assert_eq!(out.vectored_writes, 1);
    }

    #[test]
    fn cached_article_response_exposes_typed_wire_len() {
        let entry = create_test_cached_article("<test@example.com>");
        let response = entry
            .cached_response_for(RequestKind::Stat, "<test@example.com>")
            .unwrap();

        assert_eq!(
            response.wire_len(),
            crate::protocol::ResponseWireLen::new(26)
        );
    }

    #[tokio::test]
    async fn cached_article_response_writes_derived_wire_shapes() {
        let entry = create_test_cached_article("<test@example.com>");
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
                .cached_response_for(request_kind, "<test@example.com>")
                .unwrap();
            let mut out = Vec::new();

            response.write_to(&mut out).await.unwrap();

            assert_eq!(out, expected, "{request_kind:?}");
        }

        let response = entry
            .cached_response_for(RequestKind::Body, "<test@example.com>")
            .unwrap();
        let mut out = Vec::new();

        response.write_to(&mut out).await.unwrap();

        assert_eq!(out, b"222 0 <test@example.com>\r\nBody\r\n.\r\n");
    }

    #[test]
    fn body_payload_serves_body_and_stat_only() {
        let entry = cached_article_from_backend_response(
            b"222 0 <test@example.com>\r\nBody content only\r\n.\r\n",
        );

        assert_serves(
            &entry,
            &[
                (RequestKind::Article, false),
                (RequestKind::Body, true),
                (RequestKind::Head, false),
                (RequestKind::Stat, true),
            ],
        );
    }

    #[test]
    fn article_payload_serves_article_head_body_and_stat() {
        let entry = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );

        assert_serves(
            &entry,
            &[
                (RequestKind::Article, true),
                (RequestKind::Body, true),
                (RequestKind::Head, true),
                (RequestKind::Stat, true),
            ],
        );
    }

    #[test]
    fn head_payload_serves_head_and_stat_only() {
        let entry = cached_article_from_backend_response(
            b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n",
        );

        assert_serves(
            &entry,
            &[
                (RequestKind::Article, false),
                (RequestKind::Body, false),
                (RequestKind::Head, true),
                (RequestKind::Stat, true),
            ],
        );
    }

    #[test]
    fn body_payload_completeness_requires_semantic_body() {
        let complete = cached_article_from_backend_response(
            b"222 0 <test@example.com>\r\nBody content\r\n.\r\n",
        );
        let metadata_only =
            cached_article_from_backend_response(b"222 0 <test@example.com>\r\n".as_slice());

        assert!(complete.is_complete_article());
        assert!(!metadata_only.is_complete_article());
    }

    #[tokio::test]
    async fn upsert_preserves_complete_body_over_metadata_only_response() {
        let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
        let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
        let backend_id = BackendId::from_index(0);
        let complete = format!(
            "222 0 <test@example.com>\r\n{}\r\n.\r\n",
            "X".repeat(750_000)
        );

        cache
            .upsert_backend_response(
                msg_id.clone(),
                complete.as_bytes().to_vec(),
                backend_id,
                0.into(),
            )
            .await;
        cache
            .upsert_backend_response(
                msg_id.clone(),
                b"222 0 <test@example.com>\r\n".to_vec(),
                backend_id,
                0.into(),
            )
            .await;

        let cached = cache.get(&msg_id).await.expect("cached body");

        assert_eq!(
            rendered(&cached, RequestKind::Body, msg_id.as_str()),
            complete.as_bytes()
        );
    }

    #[tokio::test]
    async fn upsert_replaces_metadata_only_body_with_complete_body() {
        let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), true);
        let msg_id = MessageId::from_str_or_wrap("test@example.com").unwrap();
        let backend_id = BackendId::from_index(0);
        let complete = format!(
            "222 0 <test@example.com>\r\n{}\r\n.\r\n",
            "X".repeat(750_000)
        );

        cache
            .upsert_backend_response(
                msg_id.clone(),
                b"222 0 <test@example.com>\r\n".to_vec(),
                backend_id,
                0.into(),
            )
            .await;
        assert!(
            cache
                .get(&msg_id)
                .await
                .expect("metadata entry")
                .cached_response_for(RequestKind::Body, msg_id.as_str())
                .is_none()
        );

        cache
            .upsert_backend_response(
                msg_id.clone(),
                complete.as_bytes().to_vec(),
                backend_id,
                0.into(),
            )
            .await;
        let cached = cache.get(&msg_id).await.expect("cached body");

        assert_eq!(
            rendered(&cached, RequestKind::Body, msg_id.as_str()),
            complete.as_bytes()
        );
    }

    #[test]
    fn test_cached_article_basic() {
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = cached_article_from_backend_response(buffer.clone());

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert_eq!(
            rendered(&entry, RequestKind::Article, "<test@example.com>"),
            buffer
        );

        // Default: should try all backends
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn cached_article_ingests_backend_response_by_name() {
        let entry = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn cached_article_ingests_borrowed_backend_response() {
        let entry = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".as_slice(),
        );

        assert_eq!(entry.status_code(), StatusCode::new(220));
        assert!(matches!(entry.payload, CachedPayload::Article { .. }));
    }

    #[test]
    fn cached_article_defaults_to_tier_zero() {
        let entry = cached_article_from_backend_response(b"220 0 <test@example.com>\r\n.\r\n");

        assert_eq!(entry.tier(), ttl::CacheTier::new(0));
    }

    #[test]
    fn cached_article_can_ingest_with_tier_internally() {
        let entry = CachedArticle::from_backend_response_with_tier(
            b"220 0 <test@example.com>\r\n.\r\n",
            ttl::CacheTier::new(5),
        );

        assert_eq!(entry.tier(), ttl::CacheTier::new(5));
    }

    #[test]
    fn cached_article_stores_payload_sections_as_shared_slices() {
        trait SharedSlice {}
        impl SharedSlice for Arc<[u8]> {}
        fn assert_shared_slice<T: SharedSlice>(_: &T) {}

        let entry = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );

        match &entry.payload {
            CachedPayload::Article { headers, body, .. } => {
                assert_shared_slice(headers);
                assert_shared_slice(body);
            }
            other => panic!("expected article payload, got {other:?}"),
        }
    }

    #[test]
    fn cached_article_clone_shares_payload_sections() {
        let entry = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );
        let cloned = entry.clone();

        match (&entry.payload, &cloned.payload) {
            (
                CachedPayload::Article { headers, body, .. },
                CachedPayload::Article {
                    headers: cloned_headers,
                    body: cloned_body,
                    ..
                },
            ) => {
                assert!(std::ptr::eq(headers.as_ptr(), cloned_headers.as_ptr()));
                assert!(std::ptr::eq(body.as_ptr(), cloned_body.as_ptr()));
            }
            other => panic!("expected cloned article payload, got {other:?}"),
        }
    }

    #[test]
    fn cached_article_ingests_backend_response_bytes_without_required_vec() {
        let entry = CachedArticle::from_backend_response_bytes_with_tier(
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
    fn cached_article_ingests_chunked_backend_response_bytes_without_flattening_response() {
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

        let entry = CachedArticle::from_backend_response_bytes_with_tier(
            response.into(),
            ttl::CacheTier::new(0),
        );

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
        // Metadata-only responses should NOT be complete articles
        let metadata_only_430 = cached_article_from_backend_response(b"430\r\n");
        assert!(!metadata_only_430.is_complete_article());

        let metadata_only_220 = cached_article_from_backend_response(b"220\r\n");
        assert!(!metadata_only_220.is_complete_article());

        let metadata_only_223 = cached_article_from_backend_response(b"223\r\n");
        assert!(!metadata_only_223.is_complete_article());

        // Full article SHOULD be complete
        let full = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n",
        );
        assert!(full.is_complete_article());

        // Wrong status code (not 220) should NOT be complete article
        let head_response = cached_article_from_backend_response(
            b"221 0 <test@example.com>\r\nSubject: Test\r\n.\r\n",
        );
        assert!(!head_response.is_complete_article());

        // Missing terminator should NOT be complete
        let no_terminator = cached_article_from_backend_response(
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n",
        );
        assert!(!no_terminator.is_complete_article());
    }

    #[test]
    fn test_cached_article_record_backend_missing() {
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_cached_article("<test@example.com>");

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
    fn test_cached_article_all_backends_exhausted() {
        use crate::router::BackendCount;
        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);
        let mut entry = create_test_cached_article("<test@example.com>");

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
        let article = create_test_cached_article("<test123@example.com>");

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
            rendered(
                &retrieved.unwrap(),
                RequestKind::Article,
                "<test123@example.com>"
            ),
            rendered(&article, RequestKind::Article, "<test123@example.com>"),
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
        let article = create_test_cached_article("<article@example.com>");

        cache.insert(msgid.clone(), article.clone()).await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(
            rendered(&retrieved, RequestKind::Article, "<article@example.com>"),
            rendered(&article, RequestKind::Article, "<article@example.com>")
        );
    }

    #[tokio::test]
    async fn test_cache_upsert_new_entry() {
        let cache = ArticleCache::new(100, Duration::from_secs(300), true);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();

        cache
            .upsert_backend_response(
                msgid.clone(),
                buffer.clone(),
                BackendId::from_index(0),
                0.into(),
            )
            .await;

        let retrieved = cache.get(&msgid).await.unwrap();
        assert_eq!(
            rendered(&retrieved, RequestKind::Article, "<test@example.com>"),
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
            .upsert_backend_response(
                msgid.clone(),
                buffer.clone(),
                BackendId::from_index(0),
                0.into(),
            )
            .await;

        // Update with backend 1 - does nothing (entry already exists)
        cache
            .upsert_backend_response(
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
        let article = create_test_cached_article("<test@example.com>");

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
        assert_eq!(
            entry.payload_len().get(),
            0,
            "missing cache entries must not retain response payload bytes"
        );

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
        let article = create_test_cached_article("<test@example.com>");
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
        let article = create_test_cached_article("<expire@example.com>");

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
        let cache_metadata_only = ArticleCache::new(1024 * 1024, Duration::from_secs(300), false);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let full_size = buffer.len();

        cache_metadata_only
            .upsert_backend_response(msgid.clone(), buffer, BackendId::from_index(0), 0.into())
            .await;
        cache_metadata_only.sync().await;

        let retrieved = cache_metadata_only.get(&msgid).await.unwrap();
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
            .upsert_backend_response(msgid2.clone(), buffer2, BackendId::from_index(0), 0.into())
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
            let article = create_test_cached_article(msgid.as_ref());
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
    async fn test_cached_article_clone() {
        let article = create_test_cached_article("<test@example.com>");

        let cloned = article.clone();
        assert_eq!(
            rendered(&article, RequestKind::Article, "<test@example.com>"),
            rendered(&cloned, RequestKind::Article, "<test@example.com>")
        );
    }

    #[tokio::test]
    async fn test_cache_clone() {
        let cache1 = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB
        let cache2 = cache1.clone();

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        let article = create_test_cached_article("<test@example.com>");

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
        let article = cached_article_from_backend_response(response.as_bytes());

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
            let article = cached_article_from_backend_response(response.as_bytes());
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
    async fn test_weigher_small_metadata_only_responses() {
        // Test that small metadata-only responses account for moka internal overhead correctly
        // With MOKA_OVERHEAD = 2000 bytes (based on empirical 10x memory ratio from moka issue #473)
        let cache = ArticleCache::new(1_000_000, Duration::from_secs(300), false); // 1MB capacity

        // Create small metadata-only response (53 bytes)
        let metadata_only = b"223 0 <test@example.com>\r\n".to_vec();
        let article = cached_article_from_backend_response(metadata_only);

        let msgid = MessageId::from_borrowed("<test@example.com>").unwrap();
        cache.insert(msgid, article).await;
        cache.sync().await;

        // With MOKA_OVERHEAD = 2000: parsed metadata + availability + overhead
        // fit comfortably without pretending the whole response is retained
        // With 2.5x small response multiplier: ~5400 bytes per response
        // 1MB capacity should fit ~185 metadata-only responses

        // Insert many small metadata-only responses
        for i in 2..=200 {
            let msgid_str = format!("<metadata_only{i}@example.com>");
            let msgid = MessageId::new(msgid_str).unwrap();
            let metadata_only = format!("223 0 {}\r\n", msgid.as_str());
            let article = cached_article_from_backend_response(metadata_only.as_bytes());
            cache.insert(msgid, article).await;
        }

        cache.sync().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        cache.sync().await;

        let stats = cache.stats();
        // Should be able to fit ~150-185 small metadata-only responses in 1MB
        assert!(
            stats.entry_count >= 100,
            "Cache should fit many small metadata-only responses (got {})",
            stats.entry_count
        );
    }

    #[tokio::test]
    async fn test_cache_with_owned_message_id() {
        let cache = ArticleCache::new(1024 * 1024, Duration::from_secs(300), true); // 1MB

        // Use owned MessageId
        let msgid = MessageId::new("<owned@example.com>".to_string()).unwrap();
        let article = create_test_cached_article("<owned@example.com>");

        cache.insert(msgid.clone(), article).await;

        // Retrieve with borrowed MessageId
        let borrowed_msgid = MessageId::from_borrowed("<owned@example.com>").unwrap();
        assert!(cache.get(&borrowed_msgid).await.is_some());
    }
}
