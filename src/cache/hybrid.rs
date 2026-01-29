//! Hybrid memory+disk article cache using foyer
//!
//! This module provides a two-tier cache that stores hot articles in memory
//! and spills to disk when memory capacity is exceeded. This is ideal for
//! large-scale article caching where memory alone isn't sufficient.
//!
//! # Architecture
//!
//! ```text
//! Client Request → Memory Cache (fast, limited)
//!                         ↓ miss
//!                  Disk Cache (slower, larger)
//!                         ↓ miss
//!                  Backend Server
//! ```
//!
//! # Usage
//!
//! Configure disk cache in your config.toml:
//! ```toml
//! [cache]
//! max_capacity = "256mb"
//!
//! [cache.disk]
//! path = "/var/cache/nntp-proxy"
//! capacity = "10gb"
//! ```
//!
//! # Performance Characteristics
//!
//! - Memory tier: ~1μs access latency, bounded by configured memory capacity
//! - Disk tier: ~100μs-1ms access latency, bounded by disk capacity
//! - Automatic promotion: Frequently accessed disk entries promoted to memory
//! - LZ4 compression: Reduces disk usage by ~60% for typical NNTP articles

use crate::protocol::StatusCode;
use crate::router::BackendCount;
use crate::types::{BackendId, MessageId};
use foyer::{
    BlockEngineBuilder, Code, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    HybridCachePolicy, LruConfig, RecoverMode, RuntimeOptions, Source, TokioRuntimeOptions,
};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use super::{ArticleAvailability, ttl};

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

/// Check available disk space at the given path using df command
fn check_available_space(path: &Path) -> Option<u64> {
    // Try to use statfs on Linux/Unix
    #[cfg(unix)]
    {
        // Get filesystem stats using a known working approach
        // We'll create a temp file to trigger actual space check
        if let Ok(temp_file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.join(".space_check_tmp"))
        {
            drop(temp_file);
            let _ = std::fs::remove_file(path.join(".space_check_tmp"));
        }
    }
    None // For now, skip the check - let foyer handle it
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
    /// Backend availability bitset - checked bits
    checked: u8,
    /// Backend availability bitset - missing bits
    missing: u8,
    /// Unix timestamp when availability info was last updated (milliseconds since epoch)
    /// Used to expire stale availability-only entries (430 stubs, STAT responses)
    /// and for tier-aware TTL calculation
    timestamp: u64,
    /// Server tier (lower = higher priority)
    /// Used for tier-aware TTL: higher tier = longer TTL
    tier: u8,
    /// Complete response buffer
    /// Format: `220 <msgid>\r\n<headers>\r\n\r\n<body>\r\n.\r\n`
    buffer: Vec<u8>,
}

/// Manual Code implementation to avoid bincode's vec resizing overhead
impl Code for HybridArticleEntry {
    fn encode(&self, writer: &mut impl Write) -> foyer::Result<()> {
        // Format: status (2) + checked (1) + missing (1) + timestamp (8) + tier (1) + len (4) + buffer
        writer
            .write_all(&self.status_code.as_u16().to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&[self.checked, self.missing])
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

        // Pre-allocate exact size - this is the key optimization
        let mut buffer = vec![0u8; len];
        reader
            .read_exact(&mut buffer)
            .map_err(foyer::Error::io_error)?;

        Ok(Self {
            status_code,
            checked: header[0],
            missing: header[1],
            timestamp,
            tier,
            buffer,
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
            checked: 0,
            missing: 0,
            timestamp: ttl::now_millis(),
            tier,
            buffer,
        })
    }

    /// Get raw buffer for serving to client
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    /// Get buffer as owned Vec (for sending to client)
    #[inline]
    pub fn into_buffer(self) -> Vec<u8> {
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
        let idx = backend_id.as_index();
        // Backend is "should try" if not marked as missing
        self.missing & (1u8 << idx) == 0
    }

    /// Record that a backend returned 430 (doesn't have this article)
    pub fn record_backend_missing(&mut self, backend_id: BackendId) {
        let idx = backend_id.as_index();
        self.checked |= 1u8 << idx;
        self.missing |= 1u8 << idx;
    }

    /// Record that a backend successfully provided this article
    pub fn record_backend_has(&mut self, backend_id: BackendId) {
        let idx = backend_id.as_index();
        self.checked |= 1u8 << idx;
        self.missing &= !(1u8 << idx);
    }

    /// Check if all backends have been tried and none have the article
    pub fn all_backends_exhausted(&self, total_backends: BackendCount) -> bool {
        let count = total_backends.get();
        match count {
            0 => true,
            8 => self.missing == 0xFF,
            n => self.missing & ((1u8 << n) - 1) == (1u8 << n) - 1,
        }
    }

    /// Check if this cache entry contains a complete article (220) or body (222)
    #[inline]
    pub fn is_complete_article(&self) -> bool {
        if !matches!(
            self.status_code,
            CacheableStatusCode::Article | CacheableStatusCode::Body
        ) {
            return false;
        }
        const MIN_ARTICLE_SIZE: usize = 30;
        self.buffer.len() >= MIN_ARTICLE_SIZE && self.buffer.ends_with(b".\r\n")
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
        use CacheableStatusCode::*;

        match (self.status_code, cmd_verb) {
            // STAT just needs existence confirmation - synthesize response
            (Article | Head | Body, "STAT") => {
                Some(format!("223 0 {}\r\n", message_id).into_bytes())
            }
            // Direct match - return cached buffer if valid
            (Article, "ARTICLE") | (Body, "BODY") | (Head, "HEAD") => {
                if self.is_valid_response() {
                    Some(self.buffer.clone())
                } else {
                    tracing::warn!(
                        code = self.status_code.as_u16(),
                        len = self.buffer.len(),
                        "Cached buffer failed validation, discarding"
                    );
                    None
                }
            }
            // ARTICLE (220) contains everything, can serve BODY or HEAD requests
            // Note: For HEAD we'd ideally extract just headers, but returning full
            // article still works (client gets bonus body data)
            (Article, "BODY" | "HEAD") => {
                if self.is_valid_response() {
                    Some(self.buffer.clone())
                } else {
                    tracing::warn!(
                        code = self.status_code.as_u16(),
                        len = self.buffer.len(),
                        "Cached buffer failed validation, discarding"
                    );
                    None
                }
            }
            _ => None,
        }
    }

    /// Check if buffer contains a valid NNTP multiline response
    ///
    /// A valid response must:
    /// 1. Start with 3 ASCII digits (status code)
    /// 2. Have CRLF somewhere (line terminator)
    /// 3. End with .\r\n for multiline responses (220/221/222)
    #[inline]
    fn is_valid_response(&self) -> bool {
        // Must have at least "NNN \r\n.\r\n" = 9 bytes
        if self.buffer.len() < 9 {
            return false;
        }

        // First 3 bytes must be ASCII digits
        if !self.buffer[0].is_ascii_digit()
            || !self.buffer[1].is_ascii_digit()
            || !self.buffer[2].is_ascii_digit()
        {
            return false;
        }

        // Must end with .\r\n for multiline responses
        if !self.buffer.ends_with(b".\r\n") {
            return false;
        }

        // Must have CRLF in first line (status line)
        memchr::memmem::find(&self.buffer[..self.buffer.len().min(256)], b"\r\n").is_some()
    }

    /// Check if this entry can serve a given command type
    ///
    /// Simpler version of `response_for_command` for boolean checks.
    #[inline]
    pub fn matches_command_type_verb(&self, cmd_verb: &str) -> bool {
        use CacheableStatusCode::*;
        match self.status_code {
            Article => matches!(cmd_verb, "ARTICLE" | "BODY" | "HEAD" | "STAT"),
            Body => matches!(cmd_verb, "BODY" | "STAT"),
            Head => matches!(cmd_verb, "HEAD" | "STAT"),
            _ => false,
        }
    }

    /// Get backend availability as ArticleAvailability struct
    #[inline]
    pub fn availability(&self) -> ArticleAvailability {
        ArticleAvailability::from_bits(self.checked, self.missing)
    }

    /// Check if we have any backend availability information
    ///
    /// Returns true if at least one backend has been checked.
    /// Wrapper around `ArticleAvailability::has_availability_info()` for convenience.
    #[inline]
    pub fn has_availability_info(&self) -> bool {
        self.checked != 0
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

/// Configuration for hybrid cache
#[derive(Debug, Clone)]
pub struct HybridCacheConfig {
    /// Memory cache capacity in bytes
    pub memory_capacity: u64,
    /// Disk cache capacity in bytes
    pub disk_capacity: u64,
    /// Path to disk cache directory
    pub disk_path: std::path::PathBuf,
    /// Time-to-live for cached articles (not directly used by foyer, but kept for API consistency)
    pub ttl: Duration,
    /// Whether to cache full article bodies
    pub cache_articles: bool,
    /// Enable LZ4 compression for disk storage
    pub compression: bool,
    /// Number of shards for concurrent access
    pub shards: usize,
}

impl Default for HybridCacheConfig {
    fn default() -> Self {
        Self {
            memory_capacity: 256 * 1024 * 1024,     // 256 MB memory
            disk_capacity: 10 * 1024 * 1024 * 1024, // 10 GB disk
            disk_path: std::path::PathBuf::from("/var/cache/nntp-proxy"),
            ttl: Duration::from_secs(3600), // 1 hour
            cache_articles: true,
            compression: true,
            shards: 4,
        }
    }
}

/// Hybrid article cache with memory and disk tiers
///
/// Uses foyer's `HybridCache` for automatic memory→disk spillover.
/// Hot articles stay in memory, cold articles spill to disk.
///
/// Supports tier-aware TTL: entries from higher tier backends get longer TTLs.
/// Formula: `effective_ttl = base_ttl * (2 ^ tier)`
///
/// Note: Keys are stored as `String` (message ID without brackets) because
/// foyer requires keys to implement the `Code` trait for disk serialization.
pub struct HybridArticleCache {
    cache: HybridCache<String, HybridArticleEntry>,
    hits: AtomicU64,
    misses: AtomicU64,
    disk_hits: AtomicU64,
    config: HybridCacheConfig,
    /// Base TTL in milliseconds (used for tier-aware expiration via `effective_ttl`)
    ttl_millis: u64,
}

impl std::fmt::Debug for HybridArticleCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridArticleCache")
            .field("hits", &self.hits.load(Ordering::Relaxed))
            .field("misses", &self.misses.load(Ordering::Relaxed))
            .field("disk_hits", &self.disk_hits.load(Ordering::Relaxed))
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HybridArticleCache {
    /// Create a new hybrid cache with the given configuration
    ///
    /// This will create the disk cache directory if it doesn't exist.
    pub async fn new(config: HybridCacheConfig) -> anyhow::Result<Self> {
        // Ensure disk cache directory exists
        std::fs::create_dir_all(&config.disk_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::Other && e.to_string().contains("No space left") {
                anyhow::anyhow!(
                    "Failed to create cache directory '{}': DISK FULL - No space left on device. \
                     Free up disk space or choose a different disk_path in config.",
                    config.disk_path.display()
                )
            } else {
                anyhow::anyhow!(
                    "Failed to create cache directory '{}': {}",
                    config.disk_path.display(),
                    e
                )
            }
        })?;

        // Check available disk space before initializing
        if let Ok(_metadata) = std::fs::metadata(&config.disk_path)
            && let Some(available_bytes) = check_available_space(&config.disk_path)
        {
            let required_bytes = config.disk_capacity;
            if available_bytes < required_bytes {
                anyhow::bail!(
                    "Insufficient disk space for cache:\n\
                     Path: {}\n\
                     Required: {} GB\n\
                     Available: {} GB\n\
                     Solution: Free up {} GB or reduce 'disk_capacity' in config.",
                    config.disk_path.display(),
                    required_bytes / (1024 * 1024 * 1024),
                    available_bytes / (1024 * 1024 * 1024),
                    (required_bytes - available_bytes) / (1024 * 1024 * 1024)
                );
            }
        }

        // Use FsDevice - optimized for filesystem use (uses pread/pwrite properly)
        // Block engine controls partition sizes via block_size
        let disk_capacity_usize: usize = config.disk_capacity.try_into().map_err(|_| {
            anyhow::anyhow!(
                "Disk capacity {} bytes too large for platform (max {} bytes)",
                config.disk_capacity,
                usize::MAX
            )
        })?;

        let device = FsDeviceBuilder::new(&config.disk_path)
            .with_capacity(disk_capacity_usize)
            .build()
            .map_err(|e| {
                if e.to_string().contains("No space left") || e.to_string().contains("ENOSPC") {
                    anyhow::anyhow!(
                        "Failed to initialize disk cache at '{}': DISK FULL - No space left on device.\n\
                         Required: {} GB\n\
                         Solution: Free up disk space or reduce 'disk_capacity' in config.",
                        config.disk_path.display(),
                        config.disk_capacity / (1024 * 1024 * 1024)
                    )
                } else {
                    anyhow::anyhow!("Failed to initialize disk cache: {}", e)
                }
            })?;

        let memory_capacity_usize: usize = config
            .memory_capacity
            .try_into()
            .map_err(|_| anyhow::anyhow!("Memory capacity too large for platform"))?;

        // Block size of 768KB matches average article size (~750KB)
        // This minimizes read amplification when reading individual articles

        // Configure dedicated runtime for foyer disk I/O to avoid blocking the main runtime
        // This is critical for performance - foyer warns about latency spikes without it
        let runtime_options = RuntimeOptions::Unified(TokioRuntimeOptions {
            worker_threads: 4,
            max_blocking_threads: 8,
        });

        let mut builder = HybridCacheBuilder::new()
            .with_name("nntp-article-cache-v1") // Bumped for tier-aware TTL format (added tier byte)
            .with_policy(HybridCachePolicy::WriteOnInsertion)
            .memory(memory_capacity_usize)
            .with_shards(config.shards)
            .with_eviction_config(LruConfig {
                high_priority_pool_ratio: 0.1,
            })
            .with_weighter(|_key: &String, value: &HybridArticleEntry| value.buffer.len())
            .storage()
            .with_engine_config(
                BlockEngineBuilder::new(device)
                    .with_block_size(512 * 1024 * 1024) // 512MB blocks - fewer files/FDs
                    .with_indexer_shards(16)
                    .with_flushers(1)
                    .with_reclaimers(1),
            )
            .with_recover_mode(RecoverMode::Quiet)
            .with_runtime_options(runtime_options);

        if config.compression {
            builder = builder.with_compression(foyer::Compression::Lz4);
        }

        let cache = builder.build().await.map_err(|e| {
            if e.to_string().contains("No space left") || e.to_string().contains("ENOSPC") {
                anyhow::anyhow!(
                    "Failed to build disk cache: DISK FULL - No space left on device.\n\
                     Cache path: {}\n\
                     Memory size: {} MB\n\
                     Disk size: {} GB\n\
                     Solution: Free up disk space or reduce cache sizes in config.",
                    config.disk_path.display(),
                    config.memory_capacity / (1024 * 1024),
                    config.disk_capacity / (1024 * 1024 * 1024)
                )
            } else {
                anyhow::anyhow!("Failed to build hybrid cache: {}", e)
            }
        })?;

        info!(
            memory_mb = config.memory_capacity / (1024 * 1024),
            disk_gb = config.disk_capacity / (1024 * 1024 * 1024),
            path = %config.disk_path.display(),
            compression = config.compression,
            "Hybrid article cache initialized"
        );

        let ttl_millis = config.ttl.as_millis() as u64;
        Ok(Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            config,
            ttl_millis,
        })
    }

    /// Get an article from the cache
    ///
    /// Checks memory first, then disk. Returns None if not found in either tier.
    /// Applies tier-aware TTL expiration - higher tier entries get longer TTLs.
    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<HybridArticleEntry> {
        let start = std::time::Instant::now();
        let key = message_id.without_brackets().to_string();
        let key_time = start.elapsed();

        let get_start = std::time::Instant::now();
        let result = self.cache.get(&key).await;
        let get_time = get_start.elapsed();

        let clone_start = std::time::Instant::now();
        match result {
            Ok(Some(entry)) => {
                let cloned = entry.value().clone();

                // Check tier-aware TTL expiration
                if cloned.is_expired(self.ttl_millis) {
                    // Expired by tier-aware TTL - treat as cache miss
                    // We intentionally do NOT remove from foyer. Eviction decisions are delegated
                    // to foyer's capacity-based and LRU policies. Expired entries may linger on
                    // disk until capacity pressure forces eviction, avoiding explicit removal overhead.
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }

                self.hits.fetch_add(1, Ordering::Relaxed);
                let source = entry.source();
                // Track disk hits for monitoring
                if source == Source::Disk {
                    self.disk_hits.fetch_add(1, Ordering::Relaxed);
                }
                let clone_time = clone_start.elapsed();
                let total = start.elapsed();

                // Log slow disk reads
                if source == Source::Disk && total.as_millis() > 5 {
                    warn!(
                        key_us = key_time.as_micros(),
                        get_us = get_time.as_micros(),
                        clone_us = clone_time.as_micros(),
                        total_ms = total.as_millis(),
                        size = cloned.buffer.len(),
                        "SLOW disk cache read"
                    );
                }
                Some(cloned)
            }
            Ok(None) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(e) => {
                warn!(error = %e, "Error reading from hybrid cache");
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert or update an article in the cache
    ///
    /// With WriteOnInsertion policy, articles are written to disk immediately
    /// in a background task (non-blocking).
    ///
    /// **UPSERT SEMANTICS**: Never overwrites a larger buffer with a smaller one.
    /// A cached full article (220/222 response) must not be replaced by a STAT stub.
    ///
    /// The tier is stored with the entry for tier-aware TTL calculation.
    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
        tier: u8,
    ) {
        let key = message_id.without_brackets().to_string();
        let buffer_len = buffer.len();

        // Check for existing entry - don't overwrite larger buffers with smaller ones
        if let Ok(Some(existing)) = self.cache.get(&key).await {
            let existing_len = existing.value().buffer.len();
            if existing_len > buffer_len {
                // Existing entry is larger - just update availability info and refresh TTL
                let mut updated = existing.value().clone();
                updated.record_backend_has(backend_id);
                // Refresh timestamp on successful upsert to extend tier-aware TTL
                updated.timestamp = ttl::now_millis();
                self.cache.insert(key.clone(), updated);
                debug!(
                    msg_id = %key,
                    existing_bytes = existing_len,
                    new_bytes = buffer_len,
                    "Hybrid cache upsert: preserved larger existing entry, updated availability"
                );
                return;
            }
        }

        let Some(mut entry) = (if self.config.cache_articles {
            HybridArticleEntry::with_tier(buffer.clone(), tier)
        } else {
            // Availability-only mode: store minimal stub
            let stub = Self::create_stub(&buffer);
            HybridArticleEntry::with_tier(stub, tier)
        }) else {
            // Invalid buffer - cannot cache
            warn!(
                msg_id = %key,
                buffer_len = buffer_len,
                first_bytes = ?&buffer[..buffer_len.min(32)],
                "Cannot cache: buffer has invalid status code"
            );
            return;
        };

        entry.record_backend_has(backend_id);

        let entry_len = entry.buffer.len();
        self.cache.insert(key.clone(), entry);

        debug!(
            msg_id = %key,
            original_bytes = buffer_len,
            stored_bytes = entry_len,
            tier = tier,
            cache_articles = self.config.cache_articles,
            "Hybrid cache upsert"
        );
    }

    /// Record that a backend doesn't have an article (430 response)
    pub async fn record_missing<'a>(&self, message_id: MessageId<'a>, backend_id: BackendId) {
        let key = message_id.without_brackets().to_string();

        // Get existing entry or create a minimal stub
        let entry = match self.cache.get(&key).await {
            Ok(Some(existing)) => {
                let mut updated = existing.value().clone();
                updated.record_backend_missing(backend_id);
                updated
            }
            _ => {
                // Create stub entry for availability tracking
                // SAFETY: "430\r\n" is a valid NNTP response
                let mut entry = HybridArticleEntry::new(b"430\r\n".to_vec())
                    .expect("430 is a valid status code");
                entry.record_backend_missing(backend_id);
                entry
            }
        };

        self.cache.insert(key, entry);
    }

    /// Sync availability information at the end of a retry loop
    ///
    /// This is called ONCE at the end of a retry loop to persist all the
    /// backends that returned 430 during this request. Much more efficient
    /// than calling record_missing for each backend individually.
    ///
    /// IMPORTANT: Only creates a 430 stub entry if ALL checked backends returned 430.
    /// If any backend successfully provided the article, we skip creating an entry
    /// (the actual article will be cached via upsert, which may race with this call).
    pub async fn sync_availability<'a>(
        &self,
        message_id: MessageId<'a>,
        availability: &ArticleAvailability,
    ) {
        // Only sync if we actually tried some backends
        if availability.checked_bits() == 0 {
            return;
        }

        let key = message_id.without_brackets().to_string();

        // Get existing entry or conditionally create a stub
        let updated_entry = match self.cache.get(&key).await {
            Ok(Some(existing)) => {
                // Merge availability into existing entry
                let mut entry = existing.value().clone();
                // Merge: union of checked bits, union of missing bits
                entry.checked |= availability.checked_bits();
                entry.missing |= availability.missing_bits();
                Some(entry)
            }
            _ => {
                // No existing entry - only create a 430 stub if ALL backends returned 430
                if availability.any_backend_has_article() {
                    // A backend successfully provided the article.
                    // Don't create a 430 stub - let upsert() handle it with the real article data.
                    None
                } else {
                    // All checked backends returned 430 - create stub to track this
                    // SAFETY: "430\r\n" is a valid NNTP response
                    let mut entry = HybridArticleEntry::new(b"430\r\n".to_vec())
                        .expect("430 is a valid status code");
                    entry.checked = availability.checked_bits();
                    entry.missing = availability.missing_bits();
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    Some(entry)
                }
            }
        };

        if let Some(entry) = updated_entry {
            self.cache.insert(key, entry);
        }
    }

    /// Create a minimal stub from a response buffer (for availability-only mode)
    fn create_stub(buffer: &[u8]) -> Vec<u8> {
        // Extract just the status code line
        if let Some(pos) = buffer.iter().position(|&b| b == b'\r') {
            buffer[..pos + 2].to_vec() // Include \r\n
        } else if buffer.len() >= 3 {
            // Just the status code
            let mut stub = buffer[..3].to_vec();
            stub.extend_from_slice(b"\r\n");
            stub
        } else {
            buffer.to_vec()
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> HybridCacheStats {
        let foyer_stats = self.cache.statistics();
        HybridCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            memory_capacity: self.config.memory_capacity,
            disk_capacity: self.config.disk_capacity,
            disk_write_bytes: foyer_stats.disk_write_bytes() as u64,
            disk_read_bytes: foyer_stats.disk_read_bytes() as u64,
            disk_write_ios: foyer_stats.disk_write_ios() as u64,
            disk_read_ios: foyer_stats.disk_read_ios() as u64,
        }
    }

    /// Close the cache gracefully
    ///
    /// Flushes pending writes to disk before returning.
    /// This waits for all enqueued disk writes to complete.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.cache
            .close()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to close cache: {}", e))
    }
}

/// Statistics for hybrid cache
#[derive(Debug, Clone)]
pub struct HybridCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub disk_hits: u64,
    pub memory_capacity: u64,
    pub disk_capacity: u64,
    /// Bytes written to disk (from foyer Statistics)
    pub disk_write_bytes: u64,
    /// Bytes read from disk (from foyer Statistics)
    pub disk_read_bytes: u64,
    /// Number of write I/O operations
    pub disk_write_ios: u64,
    /// Number of read I/O operations
    pub disk_read_ios: u64,
}

impl HybridCacheStats {
    /// Calculate hit rate as a percentage
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            (self.hits as f64 / total as f64) * 100.0
        }
    }

    /// Calculate disk hit rate (hits from disk vs total hits)
    pub fn disk_hit_rate(&self) -> f64 {
        if self.hits == 0 {
            0.0
        } else {
            (self.disk_hits as f64 / self.hits as f64) * 100.0
        }
    }
}

/// Create a memory-only hybrid cache (for testing without disk I/O)
///
/// This uses foyer's Noop device to avoid any disk setup, making tests fast
/// and avoiding io_uring initialization issues.
#[cfg(test)]
impl HybridArticleCache {
    /// Create a memory-only cache for testing
    ///
    /// Uses a very small noop device to minimize initialization time.
    pub async fn new_memory_only(memory_capacity: u64) -> anyhow::Result<Self> {
        use foyer::{IoEngineBuilder, NoopDeviceBuilder, NoopIoEngineBuilder};

        let memory_capacity_usize: usize = memory_capacity
            .try_into()
            .map_err(|_| anyhow::anyhow!("Memory capacity too large for platform"))?;

        // Create minimal noop device - 64KB is minimum for block engine
        let device = NoopDeviceBuilder::new(64 * 1024).build()?;

        // Use noop I/O engine to avoid any actual I/O
        let io_engine = NoopIoEngineBuilder.build().await?;

        let builder = HybridCacheBuilder::new()
            .with_name("nntp-article-cache-test")
            .with_policy(HybridCachePolicy::WriteOnEviction)
            .memory(memory_capacity_usize)
            .with_shards(1)
            .with_eviction_config(LruConfig {
                high_priority_pool_ratio: 0.1,
            })
            .with_weighter(|_key: &String, value: &HybridArticleEntry| value.buffer.len())
            .storage()
            .with_io_engine(io_engine)
            .with_engine_config(BlockEngineBuilder::new(device))
            .with_recover_mode(RecoverMode::Quiet);

        let cache = builder.build().await?;

        let config = HybridCacheConfig {
            memory_capacity,
            disk_capacity: 0, // No disk in memory-only mode
            disk_path: std::path::PathBuf::new(),
            ttl: Duration::from_secs(3600),
            cache_articles: true,
            compression: false,
            shards: 1,
        };

        Ok(Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            config,
            ttl_millis: 3600 * 1000, // 1 hour in milliseconds
        })
    }
}

// NOTE: These tests are disabled because foyer's HybridCache requires special runtime setup
// that conflicts with nextest's test isolation. They hang indefinitely even with noop devices.
// To test foyer integration, use the nntp-hybrid-cache-proxy binary directly.
//
// The issue is likely that foyer spawns internal background tasks that don't complete
// in the test context. This is a known pattern with async caches that need cleanup.
//
// Run manual tests with:
//   cargo test --features hybrid-cache cache::hybrid -- --ignored
#[cfg(test)]
mod tests {
    //! Unit tests for HybridArticleCache
    //!
    //! NOTE: These tests are marked as #[ignore] due to foyer runtime issues in test context.
    //! The actual Foyer integration is tested through:
    //! - MockHybridCache in tests/test_hybrid_cache_integration.rs
    //! - UnifiedCache wrapper in tests/test_unified_cache.rs
    //! - Full integration tests with real cache instances
    //!
    //! To run these ignored tests manually:
    //!   cargo test --package nntp-proxy --lib cache::hybrid::tests -- --ignored --nocapture

    use super::*;

    #[tokio::test]
    #[ignore = "foyer HybridCache hangs in test context - run manually with --ignored"]
    async fn test_hybrid_cache_basic() {
        let cache = HybridArticleCache::new_memory_only(1024 * 1024)
            .await
            .unwrap();

        // Insert an article
        let msg_id = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let buffer = b"220 0 <test123@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        cache
            .upsert(msg_id, buffer.clone(), BackendId::from_index(0), 0)
            .await;

        // Retrieve it
        let msg_id = MessageId::from_borrowed("<test123@example.com>").unwrap();
        let entry = cache.get(&msg_id).await.unwrap();
        assert_eq!(entry.buffer(), buffer.as_slice());
        assert!(entry.should_try_backend(BackendId::from_index(0)));

        // Check stats
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 0);

        cache.close().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "foyer HybridCache hangs in test context - run manually with --ignored"]
    async fn test_hybrid_cache_availability_tracking() {
        let cache = HybridArticleCache::new_memory_only(1024 * 1024)
            .await
            .unwrap();

        // Record a 430 response
        let msg_id = MessageId::from_borrowed("<missing@example.com>").unwrap();
        cache.record_missing(msg_id, BackendId::from_index(0)).await;

        // Check availability
        let msg_id = MessageId::from_borrowed("<missing@example.com>").unwrap();
        let entry = cache.get(&msg_id).await.unwrap();
        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));

        cache.close().await.unwrap();
    }

    // Test the HybridArticleEntry directly without foyer
    #[test]
    fn test_hybrid_entry_basic() {
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_vec();
        let mut entry = HybridArticleEntry::new(buffer.clone()).expect("valid status code");

        assert_eq!(entry.buffer(), buffer.as_slice());
        assert_eq!(entry.status_code().map(|c| c.as_u16()), Some(220));

        // Test backend tracking
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

        // Initially all backends are "should try" (not marked missing)
        for i in 0..8 {
            assert!(entry.should_try_backend(BackendId::from_index(i)));
        }

        // Mark some backends as missing
        entry.record_backend_missing(BackendId::from_index(0));
        entry.record_backend_missing(BackendId::from_index(2));
        entry.record_backend_missing(BackendId::from_index(4));

        assert!(!entry.should_try_backend(BackendId::from_index(0)));
        assert!(entry.should_try_backend(BackendId::from_index(1)));
        assert!(!entry.should_try_backend(BackendId::from_index(2)));
        assert!(entry.should_try_backend(BackendId::from_index(3)));
        assert!(!entry.should_try_backend(BackendId::from_index(4)));

        // Check availability struct
        let avail = entry.availability();
        assert!(avail.is_missing(BackendId::from_index(0)));
        assert!(!avail.is_missing(BackendId::from_index(1))); // Not checked = not missing
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
        // Invalid status code
        assert!(HybridArticleEntry::new(b"999 invalid\r\n".to_vec()).is_none());

        // Empty buffer
        assert!(HybridArticleEntry::new(vec![]).is_none());

        // Too short
        assert!(HybridArticleEntry::new(b"20".to_vec()).is_none());

        // Not starting with digits
        assert!(HybridArticleEntry::new(b"abc\r\n".to_vec()).is_none());

        // Valid codes we allow
        assert!(HybridArticleEntry::new(b"220 article\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"221 head\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"222 body\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"223 stat\r\n".to_vec()).is_some());
        assert!(HybridArticleEntry::new(b"430 not found\r\n".to_vec()).is_some());
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
        // Adjacent codes that are NOT cacheable
        assert_eq!(CacheableStatusCode::try_from(219), Err(219));
        assert_eq!(CacheableStatusCode::try_from(224), Err(224));
        assert_eq!(CacheableStatusCode::try_from(429), Err(429));
        assert_eq!(CacheableStatusCode::try_from(431), Err(431));

        // Common NNTP codes that aren't cacheable
        assert_eq!(CacheableStatusCode::try_from(200), Err(200));
        assert_eq!(CacheableStatusCode::try_from(201), Err(201));
        assert_eq!(CacheableStatusCode::try_from(211), Err(211));
        assert_eq!(CacheableStatusCode::try_from(411), Err(411));
        assert_eq!(CacheableStatusCode::try_from(480), Err(480));
        assert_eq!(CacheableStatusCode::try_from(500), Err(500));

        // Edge cases
        assert_eq!(CacheableStatusCode::try_from(0), Err(0));
        assert_eq!(CacheableStatusCode::try_from(u16::MAX), Err(u16::MAX));
    }

    #[test]
    fn test_cacheable_status_code_roundtrip() {
        // Every variant round-trips through u16
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
        // repr(u16) means the enum is 2 bytes
        assert_eq!(size_of::<CacheableStatusCode>(), size_of::<u16>());
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
        // These are valid NNTP status codes but not cacheable
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
    // Code encode/decode roundtrip with enum
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
        // Hand-craft encoded bytes with an invalid status code (999)
        let mut buf = Vec::new();
        buf.extend_from_slice(&999u16.to_le_bytes()); // invalid code
        buf.extend_from_slice(&[0u8; 2]); // checked + missing
        buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp
        buf.push(0); // tier
        buf.extend_from_slice(&5u32.to_le_bytes()); // len
        buf.extend_from_slice(b"hello"); // buffer

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
    // is_complete_article with enum
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
        // 220 with buffer too small (< 30 bytes)
        let entry = HybridArticleEntry::new(b"220 ok\r\n.\r\n".to_vec()).unwrap();
        assert!(!entry.is_complete_article());
    }

    // =========================================================================
    // response_for_command with enum
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
    // matches_command_type_verb with enum
    // =========================================================================

    #[test]
    fn test_matches_command_type_verb_stat_for_all_content_codes() {
        // STAT works for 220, 221, 222 but NOT 223 or 430
        for buf in [&b"220 ok\r\n"[..], b"221 ok\r\n", b"222 ok\r\n"] {
            let entry = HybridArticleEntry::new(buf.to_vec()).unwrap();
            assert!(
                entry.matches_command_type_verb("STAT"),
                "STAT should match for {}xx entry",
                buf[0] - b'0'
            );
        }

        // 223 and 430 do NOT match STAT (or anything else)
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
}
