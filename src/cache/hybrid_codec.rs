//! Hybrid cache entry type and foyer codec
//!
//! Contains `HybridArticleEntry` and its manual `Code` implementation for
//! efficient serialization to/from foyer's disk cache.
//!
//! # Wire Format
//!
//! ```text
//! [status:u16][checked:u8][missing:u8][timestamp:u64][tier:u8][len:u32][buffer:bytes]
//! ```

use crate::protocol::StatusCode;
use crate::router::BackendCount;
use crate::types::BackendId;
use foyer::Code;
use std::io::{Read, Write};
use std::sync::Arc;

use super::availability::ArticleAvailability;
use super::ttl;

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
/// - Simple binary format: [status:u16][checked:u8][missing:u8][timestamp:u64][tier:u8][len:u32][buffer:bytes]
#[derive(Clone, Debug)]
pub struct HybridArticleEntry {
    /// Validated NNTP status code — only cacheable codes are representable
    status_code: CacheableStatusCode,
    /// Backend availability tracking (checked/missing bitsets)
    pub(super) availability: ArticleAvailability,
    /// Unix timestamp when availability info was last updated (milliseconds since epoch)
    /// Used to expire stale availability-only entries (430 stubs, STAT responses)
    /// and for tier-aware TTL calculation
    pub(super) timestamp: u64,
    /// Server tier (lower = higher priority)
    /// Used for tier-aware TTL: higher tier = longer TTL
    tier: u8,
    /// Complete response buffer (Arc for O(1) clone on cache hits)
    /// Format: `220 <msgid>\r\n<headers>\r\n\r\n<body>\r\n.\r\n`
    pub(super) buffer: Arc<Vec<u8>>,
}

/// Manual Code implementation to avoid bincode's vec resizing overhead
impl Code for HybridArticleEntry {
    fn encode(&self, writer: &mut impl Write) -> foyer::Result<()> {
        // Format: status (2) + checked (1) + missing (1) + timestamp (8) + tier (1) + len (4) + buffer
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
            .write_all(&self.timestamp.to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&[self.tier])
            .map_err(foyer::Error::io_error)?;
        let len = self.buffer.len() as u32;
        writer
            .write_all(&len.to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&self.buffer)
            .map_err(foyer::Error::io_error)?;
        Ok(())
    }

    fn decode(reader: &mut impl Read) -> foyer::Result<Self> {
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
                format!("Invalid cached status code: {}", code),
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
        let timestamp = u64::from_le_bytes(timestamp_bytes);

        // Read tier
        let mut tier_byte = [0u8; 1];
        reader
            .read_exact(&mut tier_byte)
            .map_err(foyer::Error::io_error)?;
        let tier = tier_byte[0];

        // Read length and pre-allocate buffer (no resizing!)
        let mut len_bytes = [0u8; 4];
        reader
            .read_exact(&mut len_bytes)
            .map_err(foyer::Error::io_error)?;
        let len = u32::from_le_bytes(len_bytes) as usize;

        // Reject unreasonably large cached entries to avoid OOM on corrupted data.
        const MAX_BUFFER_SIZE: usize = 100 * 1024 * 1024; // 100 MiB hard limit
        if len > MAX_BUFFER_SIZE {
            return Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Cached article too large: {} bytes (max {} bytes)",
                    len, MAX_BUFFER_SIZE
                ),
            )));
        }

        // Read exactly len bytes without pre-zeroing (idiomatic, safe).
        // Use take() to limit reads to len bytes, then collect into Vec.
        use std::io::Read;
        let mut buffer = Vec::new();
        reader
            .take(len as u64)
            .read_to_end(&mut buffer)
            .map_err(foyer::Error::io_error)?;
        if buffer.len() != len {
            return Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Expected {} bytes, got {}", len, buffer.len()),
            )));
        }

        Ok(Self {
            status_code,
            availability: ArticleAvailability::from_bits(header[0], header[1]),
            timestamp,
            tier,
            buffer: Arc::new(buffer),
        })
    }

    fn estimated_size(&self) -> usize {
        2 + 2 + 8 + 1 + 4 + self.buffer.len() // status + header + timestamp + tier + len + buffer
    }
}

impl HybridArticleEntry {
    /// Create from response buffer - returns None if buffer has invalid status code
    ///
    /// This is the ONLY way to create an entry. Invalid buffers are rejected.
    /// Tier defaults to 0.
    pub fn new(buffer: Vec<u8>) -> Option<Self> {
        Self::with_tier(buffer, 0)
    }

    /// Create from response buffer with specified tier
    ///
    /// Returns None if buffer has invalid or non-cacheable status code.
    pub fn with_tier(buffer: Vec<u8>, tier: u8) -> Option<Self> {
        let raw_code = StatusCode::parse(&buffer)?.as_u16();
        let status_code = CacheableStatusCode::try_from(raw_code).ok()?;

        Some(Self {
            status_code,
            availability: ArticleAvailability::new(),
            timestamp: ttl::now_millis(),
            tier,
            buffer: Arc::new(buffer),
        })
    }

    /// Get raw buffer for serving to client
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    /// Get buffer as shared Arc (O(1) clone for sending to client)
    #[inline]
    pub fn into_buffer(self) -> Arc<Vec<u8>> {
        self.buffer
    }

    /// Get the validated status code
    ///
    /// This always returns a valid code because entries cannot be created
    /// with invalid status codes. Returns Option for API consistency with ArticleEntry.
    #[inline]
    pub fn status_code(&self) -> Option<StatusCode> {
        // SAFETY: Invariant enforced by new() and decode()
        Some(StatusCode::new(self.status_code.as_u16()))
    }

    /// Check if we should try fetching from this backend
    #[inline]
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

    /// Check if all backends have been tried and none have the article
    pub fn all_backends_exhausted(&self, total_backends: BackendCount) -> bool {
        self.availability.all_exhausted(total_backends)
    }

    /// Check if this cache entry contains a complete article (220) or body (222)
    #[inline]
    pub fn is_complete_article(&self) -> bool {
        super::entry_helpers::is_complete_article(&self.buffer, self.status_code.as_u16())
    }

    /// Get the appropriate response for a command, if this cache entry can serve it
    ///
    /// Returns `Some(response_bytes)` if cache can satisfy the command:
    /// - ARTICLE (220 cached) → returns full cached response
    /// - BODY (222 cached or 220 cached) → returns cached response
    /// - HEAD (221 cached or 220 cached) → returns cached response
    /// - STAT → synthesizes "223 0 <msg-id>\r\n" (we know article exists)
    ///
    /// Returns `None` if cached response can't serve this command type.
    pub fn response_for_command(&self, cmd_verb: &str, message_id: &str) -> Option<Vec<u8>> {
        super::entry_helpers::response_for_command(
            &self.buffer,
            self.status_code.as_u16(),
            cmd_verb,
            message_id,
        )
    }

    /// Check if this entry can serve a given command type
    ///
    /// Simpler version of `response_for_command` for boolean checks.
    #[inline]
    pub fn matches_command_type_verb(&self, cmd_verb: &str) -> bool {
        super::entry_helpers::matches_command_type_verb(self.status_code.as_u16(), cmd_verb)
    }

    /// Get backend availability as ArticleAvailability struct
    #[inline]
    pub fn availability(&self) -> ArticleAvailability {
        self.availability
    }

    /// Check if we have any backend availability information
    ///
    /// Returns true if at least one backend has been checked.
    /// Wrapper around `ArticleAvailability::has_availability_info()` for convenience.
    #[inline]
    pub fn has_availability_info(&self) -> bool {
        self.availability.has_availability_info()
    }

    /// Check if availability information is stale (older than ttl_millis)
    ///
    /// HybridArticleEntry stores timestamps for tier-aware TTL, but foyer's cache
    /// handles eviction based on insertion time. This method is kept for compatibility
    /// and always returns false since the cache layer manages staleness.
    #[inline]
    pub fn is_availability_stale(&self, _ttl_millis: u64) -> bool {
        // Foyer cache handles TTL-based eviction separately
        false
    }

    /// Clear stale availability information
    ///
    /// HybridArticleEntry now tracks timestamps via `timestamp` field for tier-aware TTL,
    /// but foyer handles eviction separately. This method is a no-op for compatibility.
    #[inline]
    pub fn clear_stale_availability(&mut self, _ttl_millis: u64) {
        // Foyer cache handles TTL-based eviction, no need to clear here
    }

    /// Check if this entry has expired based on tier-aware TTL
    ///
    /// See [`super::ttl`] for the TTL formula.
    #[inline]
    pub fn is_expired(&self, base_ttl_millis: u64) -> bool {
        ttl::is_expired(self.timestamp, base_ttl_millis, self.tier)
    }

    /// Get the tier of the backend that provided this article
    #[inline]
    pub fn tier(&self) -> u8 {
        self.tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendId;

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

    // =========================================================================
    // HybridArticleEntry tests
    // =========================================================================

    #[test]
    fn test_hybrid_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let mut entry = HybridArticleEntry::new(buffer.clone()).expect("valid status code");

        assert_eq!(entry.buffer(), buffer.as_slice());
        assert_eq!(entry.status_code().map(|c| c.as_u16()), Some(220));

        entry.record_backend_has(BackendId::from_index(0));
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));

        entry.record_backend_missing(BackendId::from_index(1));
        assert!(entry.should_try_backend(BackendId::from_index(0)));
        assert!(!entry.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn test_hybrid_entry_availability() {
        let mut entry = HybridArticleEntry::new(b"220 ok\r\n".to_vec()).expect("valid");

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
        let article = HybridArticleEntry::new(b"220 0 <id>\r\n".to_vec()).expect("valid");
        assert!(article.matches_command_type_verb("ARTICLE"));
        assert!(article.matches_command_type_verb("BODY"));
        assert!(article.matches_command_type_verb("HEAD"));

        let body = HybridArticleEntry::new(b"222 0 <id>\r\n".to_vec()).expect("valid");
        assert!(!body.matches_command_type_verb("ARTICLE"));
        assert!(body.matches_command_type_verb("BODY"));
        assert!(!body.matches_command_type_verb("HEAD"));

        let head = HybridArticleEntry::new(b"221 0 <id>\r\n".to_vec()).expect("valid");
        assert!(!head.matches_command_type_verb("ARTICLE"));
        assert!(!head.matches_command_type_verb("BODY"));
        assert!(head.matches_command_type_verb("HEAD"));
    }

    #[test]
    fn test_hybrid_entry_rejects_invalid() {
        assert!(HybridArticleEntry::new(b"999 invalid\r\n".to_vec()).is_none());
        assert!(HybridArticleEntry::new(vec![]).is_none());
        assert!(HybridArticleEntry::new(b"20".to_vec()).is_none());
        assert!(HybridArticleEntry::new(b"abc\r\n".to_vec()).is_none());

        assert!(HybridArticleEntry::new(b"220 article\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"221 head\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"222 body\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"223 stat\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"430 not found\r\n".to_vec()).is_some());
    }

    // =========================================================================
    // Entry status_code field uses enum
    // =========================================================================

    #[test]
    fn test_entry_status_code_returns_protocol_status_code() {
        let entry = HybridArticleEntry::new(b"220 0 <id>\r\n".to_vec()).unwrap();
        let sc = entry.status_code().unwrap();
        assert_eq!(sc.as_u16(), 220);

        let entry = HybridArticleEntry::new(b"430 not found\r\n".to_vec()).unwrap();
        let sc = entry.status_code().unwrap();
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
            let entry = HybridArticleEntry::new(buf.to_vec())
                .unwrap_or_else(|| panic!("should accept code {}", expected));
            assert_eq!(entry.status_code().unwrap().as_u16(), *expected);
        }
    }

    #[test]
    fn test_entry_rejects_non_cacheable_nntp_codes() {
        for code in [200, 201, 211, 411, 480, 500, 502] {
            let buf = format!("{} response\r\n", code).into_bytes();
            assert!(
                HybridArticleEntry::new(buf).is_none(),
                "code {} should be rejected",
                code
            );
        }
    }

    // =========================================================================
    // Code encode/decode roundtrip
    // =========================================================================

    #[test]
    fn test_code_encode_decode_roundtrip_article() {
        let entry =
            HybridArticleEntry::new(b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec())
                .unwrap();
        let mut buf = Vec::new();
        entry.encode(&mut buf).unwrap();
        let decoded = HybridArticleEntry::decode(&mut buf.as_slice()).unwrap();

        assert_eq!(decoded.status_code().unwrap().as_u16(), 220);
        assert_eq!(decoded.buffer(), entry.buffer());
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
            let entry = HybridArticleEntry::new(raw.to_vec()).unwrap();
            let mut encoded = Vec::new();
            entry.encode(&mut encoded).unwrap();
            let decoded = HybridArticleEntry::decode(&mut encoded.as_slice()).unwrap();
            assert_eq!(
                decoded.status_code().unwrap().as_u16(),
                entry.status_code().unwrap().as_u16()
            );
            assert_eq!(decoded.buffer(), entry.buffer());
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
        let entry = HybridArticleEntry::with_tier(b"220 article\r\n".to_vec(), 3).unwrap();
        assert_eq!(entry.tier(), 3);

        let mut encoded = Vec::new();
        entry.encode(&mut encoded).unwrap();
        let decoded = HybridArticleEntry::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.tier(), 3);
    }

    #[test]
    fn test_code_encode_decode_preserves_availability() {
        let mut entry = HybridArticleEntry::new(b"220 ok\r\n".to_vec()).unwrap();
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
        let entry = HybridArticleEntry::new(b"220 article\r\n".to_vec()).unwrap();
        let expected = 2 + 2 + 8 + 1 + 4 + entry.buffer().len();
        assert_eq!(entry.estimated_size(), expected);
    }

    // =========================================================================
    // is_complete_article
    // =========================================================================

    #[test]
    fn test_is_complete_article_220() {
        let entry =
            HybridArticleEntry::new(b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec())
                .unwrap();
        assert!(entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_222() {
        let entry =
            HybridArticleEntry::new(b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n".to_vec()).unwrap();
        assert!(entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_head() {
        let entry =
            HybridArticleEntry::new(b"221 0 <t@x>\r\nSubject: T\r\n.\r\n".to_vec()).unwrap();
        assert!(!entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_stat() {
        let entry = HybridArticleEntry::new(b"223 0 <t@x>\r\n".to_vec()).unwrap();
        assert!(!entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_430() {
        let entry = HybridArticleEntry::new(b"430 not found\r\n".to_vec()).unwrap();
        assert!(!entry.is_complete_article());
    }

    #[test]
    fn test_is_complete_article_false_for_too_small_buffer() {
        let entry = HybridArticleEntry::new(b"220 ok\r\n.\r\n".to_vec()).unwrap();
        assert!(!entry.is_complete_article());
    }

    // =========================================================================
    // response_for_command
    // =========================================================================

    #[test]
    fn test_response_for_command_stat_from_220() {
        let entry =
            HybridArticleEntry::new(b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec())
                .unwrap();
        let resp = entry
            .response_for_command("STAT", "<t@x>")
            .expect("should serve STAT");
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_response_for_command_stat_from_221() {
        let entry =
            HybridArticleEntry::new(b"221 0 <t@x>\r\nSubject: T\r\n.\r\n".to_vec()).unwrap();
        let resp = entry
            .response_for_command("STAT", "<t@x>")
            .expect("should serve STAT from head");
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_response_for_command_stat_from_222() {
        let entry =
            HybridArticleEntry::new(b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n".to_vec()).unwrap();
        let resp = entry
            .response_for_command("STAT", "<t@x>")
            .expect("should serve STAT from body");
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_response_for_command_stat_not_from_430() {
        let entry = HybridArticleEntry::new(b"430 not found\r\n".to_vec()).unwrap();
        assert!(entry.response_for_command("STAT", "<t@x>").is_none());
    }

    #[test]
    fn test_response_for_command_article_direct() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = HybridArticleEntry::new(buf.clone()).unwrap();
        let resp = entry
            .response_for_command("ARTICLE", "<t@x>")
            .expect("should serve ARTICLE");
        assert_eq!(resp, buf);
    }

    #[test]
    fn test_response_for_command_body_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = HybridArticleEntry::new(buf.clone()).unwrap();
        let resp = entry
            .response_for_command("BODY", "<t@x>")
            .expect("220 can serve BODY");
        assert_eq!(resp, buf);
    }

    #[test]
    fn test_response_for_command_head_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec();
        let entry = HybridArticleEntry::new(buf.clone()).unwrap();
        let resp = entry
            .response_for_command("HEAD", "<t@x>")
            .expect("220 can serve HEAD");
        assert_eq!(resp, buf);
    }

    #[test]
    fn test_response_for_command_body_cannot_serve_article() {
        let entry =
            HybridArticleEntry::new(b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n".to_vec()).unwrap();
        assert!(entry.response_for_command("ARTICLE", "<t@x>").is_none());
    }

    #[test]
    fn test_response_for_command_head_cannot_serve_body() {
        let entry =
            HybridArticleEntry::new(b"221 0 <t@x>\r\nSubject: T\r\n.\r\n".to_vec()).unwrap();
        assert!(entry.response_for_command("BODY", "<t@x>").is_none());
    }

    #[test]
    fn test_response_for_command_unknown_verb() {
        let entry =
            HybridArticleEntry::new(b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n".to_vec())
                .unwrap();
        assert!(entry.response_for_command("LIST", "<t@x>").is_none());
        assert!(entry.response_for_command("GROUP", "<t@x>").is_none());
        assert!(entry.response_for_command("QUIT", "<t@x>").is_none());
    }

    // =========================================================================
    // matches_command_type_verb
    // =========================================================================

    #[test]
    fn test_matches_command_type_verb_stat_for_all_content_codes() {
        for buf in [&b"220 ok\r\n"[..], b"221 ok\r\n", b"222 ok\r\n"] {
            let entry = HybridArticleEntry::new(buf.to_vec()).unwrap();
            assert!(
                entry.matches_command_type_verb("STAT"),
                "STAT should match for {}xx entry",
                buf[0] - b'0'
            );
        }

        let stat_entry = HybridArticleEntry::new(b"223 stat\r\n".to_vec()).unwrap();
        assert!(!stat_entry.matches_command_type_verb("STAT"));

        let missing_entry = HybridArticleEntry::new(b"430 missing\r\n".to_vec()).unwrap();
        assert!(!missing_entry.matches_command_type_verb("STAT"));
    }

    #[test]
    fn test_matches_command_type_verb_430_matches_nothing() {
        let entry = HybridArticleEntry::new(b"430 missing\r\n".to_vec()).unwrap();
        assert!(!entry.matches_command_type_verb("ARTICLE"));
        assert!(!entry.matches_command_type_verb("HEAD"));
        assert!(!entry.matches_command_type_verb("BODY"));
        assert!(!entry.matches_command_type_verb("STAT"));
    }

    #[test]
    fn test_matches_command_type_verb_223_matches_nothing() {
        let entry = HybridArticleEntry::new(b"223 stat\r\n".to_vec()).unwrap();
        assert!(!entry.matches_command_type_verb("ARTICLE"));
        assert!(!entry.matches_command_type_verb("HEAD"));
        assert!(!entry.matches_command_type_verb("BODY"));
        assert!(!entry.matches_command_type_verb("STAT"));
    }

    #[test]
    fn test_with_tier_sets_tier() {
        let entry = HybridArticleEntry::with_tier(b"220 ok\r\n".to_vec(), 5).unwrap();
        assert_eq!(entry.tier(), 5);
    }

    #[test]
    fn test_with_tier_zero_default() {
        let entry = HybridArticleEntry::new(b"220 ok\r\n".to_vec()).unwrap();
        assert_eq!(entry.tier(), 0);
    }

    #[test]
    fn test_with_tier_rejects_invalid_code() {
        assert!(HybridArticleEntry::with_tier(b"999 bad\r\n".to_vec(), 0).is_none());
    }

    // =========================================================================
    // Property tests for HybridArticleEntry codec
    // =========================================================================

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_220() {
        let original = HybridArticleEntry::new(
            b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n".to_vec(),
        )
        .unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_eq!(original.buffer(), decoded.buffer());
        assert_eq!(original.tier(), decoded.tier());
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_221() {
        let original = HybridArticleEntry::new(
            b"221 headers\r\nMid: <test@example.com>\r\n\r\n.\r\n".to_vec(),
        )
        .unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_eq!(original.buffer(), decoded.buffer());
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_222() {
        let original =
            HybridArticleEntry::new(b"222 body\r\n\r\nbody content\r\n.\r\n".to_vec()).unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_eq!(original.buffer(), decoded.buffer());
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_223() {
        let original = HybridArticleEntry::new(b"223 stat\r\n.\r\n".to_vec()).unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_eq!(original.buffer(), decoded.buffer());
    }

    #[test]
    fn prop_hybrid_article_encode_decode_roundtrip_430() {
        let original = HybridArticleEntry::new(b"430 missing\r\n.\r\n".to_vec()).unwrap();

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let mut reader = std::io::Cursor::new(buffer);
        let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

        assert_eq!(original.buffer(), decoded.buffer());
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
            let entry = HybridArticleEntry::new(code.to_vec()).unwrap();
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
            let entry = HybridArticleEntry::with_tier(
                b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n".to_vec(),
                tier,
            )
            .unwrap();

            let mut buffer = Vec::new();
            entry.encode(&mut buffer).unwrap();

            let mut reader = std::io::Cursor::new(buffer);
            let decoded = HybridArticleEntry::decode(&mut reader).unwrap();

            assert_eq!(tier, decoded.tier(), "Tier mismatch");
        }
    }

    #[test]
    fn prop_hybrid_article_preserves_availability() {
        let entry = HybridArticleEntry::new(
            b"220 article\r\nMid: <test@example.com>\r\n\r\nbody\r\n.\r\n".to_vec(),
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
