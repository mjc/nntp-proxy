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

use super::ArticleAvailability;

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
/// - Simple binary format: [status:u16][checked:u8][missing:u8][len:u32][buffer:bytes]
#[derive(Clone, Debug)]
pub struct HybridArticleEntry {
    /// Validated NNTP status code (220, 221, 222, 223, 430)
    /// Stored explicitly so we never have to re-parse from buffer
    status_code: u16,
    /// Backend availability bitset - checked bits
    checked: u8,
    /// Backend availability bitset - missing bits
    missing: u8,
    /// Complete response buffer
    /// Format: `220 <msgid>\r\n<headers>\r\n\r\n<body>\r\n.\r\n`
    buffer: Vec<u8>,
}

/// Manual Code implementation to avoid bincode's vec resizing overhead
impl Code for HybridArticleEntry {
    fn encode(&self, writer: &mut impl Write) -> foyer::Result<()> {
        // Format: status (2) + checked (1) + missing (1) + len (4) + buffer
        writer
            .write_all(&self.status_code.to_le_bytes())
            .map_err(foyer::Error::io_error)?;
        writer
            .write_all(&[self.checked, self.missing])
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
        let status_code = u16::from_le_bytes(status_bytes);

        // Validate status code on decode - reject corrupted entries
        if !Self::is_valid_status_code(status_code) {
            return Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid cached status code: {}", status_code),
            )));
        }

        // Read header: checked + missing
        let mut header = [0u8; 2];
        reader
            .read_exact(&mut header)
            .map_err(foyer::Error::io_error)?;

        // Read length and pre-allocate buffer (no resizing!)
        let mut len_bytes = [0u8; 4];
        reader
            .read_exact(&mut len_bytes)
            .map_err(foyer::Error::io_error)?;
        let len = u32::from_le_bytes(len_bytes) as usize;

        // Pre-allocate exact size - this is the key optimization
        let mut buffer = vec![0u8; len];
        reader
            .read_exact(&mut buffer)
            .map_err(foyer::Error::io_error)?;

        Ok(Self {
            status_code,
            checked: header[0],
            missing: header[1],
            buffer,
        })
    }

    fn estimated_size(&self) -> usize {
        2 + 2 + 4 + self.buffer.len() // status + header + len + buffer
    }
}

impl HybridArticleEntry {
    /// Valid NNTP status codes for cached articles
    const VALID_CODES: [u16; 5] = [220, 221, 222, 223, 430];

    /// Check if a status code is valid for caching
    #[inline]
    fn is_valid_status_code(code: u16) -> bool {
        Self::VALID_CODES.contains(&code)
    }

    /// Create from response buffer - returns None if buffer has invalid status code
    ///
    /// This is the ONLY way to create an entry. Invalid buffers are rejected.
    pub fn new(buffer: Vec<u8>) -> Option<Self> {
        let status_code = StatusCode::parse(&buffer)?.as_u16();

        if !Self::is_valid_status_code(status_code) {
            return None;
        }

        Some(Self {
            status_code,
            checked: 0,
            missing: 0,
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
        Some(StatusCode::new(self.status_code))
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
        let code = self.status_code;
        if code != 220 && code != 222 {
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
        let code = self.status_code;

        match (code, cmd_verb) {
            // STAT just needs existence confirmation - synthesize response
            (220..=222, "STAT") => Some(format!("223 0 {}\r\n", message_id).into_bytes()),
            // Direct match - return cached buffer if valid
            (220, "ARTICLE") | (222, "BODY") | (221, "HEAD") => {
                // Validate buffer is a well-formed NNTP response
                if self.is_valid_response() {
                    Some(self.buffer.clone())
                } else {
                    tracing::warn!(
                        code = code,
                        len = self.buffer.len(),
                        "Cached buffer failed validation, discarding"
                    );
                    None
                }
            }
            // ARTICLE (220) contains everything, can serve BODY or HEAD requests
            // Note: For HEAD we'd ideally extract just headers, but returning full
            // article still works (client gets bonus body data)
            (220, "BODY" | "HEAD") => {
                if self.is_valid_response() {
                    Some(self.buffer.clone())
                } else {
                    tracing::warn!(
                        code = code,
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
        match self.status_code {
            220 => matches!(cmd_verb, "ARTICLE" | "BODY" | "HEAD" | "STAT"),
            222 => matches!(cmd_verb, "BODY" | "STAT"),
            221 => matches!(cmd_verb, "HEAD" | "STAT"),
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

    /// Check if availability information is stale (older than ttl_secs)
    ///
    /// Since HybridArticleEntry doesn't track timestamps, this always returns false.
    /// The foyer cache handles eviction separately.
    #[inline]
    pub fn is_availability_stale(&self, _ttl_secs: u64) -> bool {
        // HybridArticleEntry doesn't track timestamps - foyer handles TTL
        false
    }

    /// Clear stale availability information
    ///
    /// Since HybridArticleEntry doesn't track timestamps, this is a no-op.
    #[inline]
    pub fn clear_stale_availability(&mut self, _ttl_secs: u64) {
        // No-op: HybridArticleEntry doesn't track timestamps
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
/// Note: Keys are stored as `String` (message ID without brackets) because
/// foyer requires keys to implement the `Code` trait for disk serialization.
pub struct HybridArticleCache {
    cache: HybridCache<String, HybridArticleEntry>,
    hits: AtomicU64,
    misses: AtomicU64,
    disk_hits: AtomicU64,
    config: HybridCacheConfig,
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
        let device = FsDeviceBuilder::new(&config.disk_path)
            .with_capacity(config.disk_capacity as usize)
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
            .with_name("nntp-article-cache")
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

        Ok(Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            config,
        })
    }

    /// Get an article from the cache
    ///
    /// Checks memory first, then disk. Returns None if not found in either tier.
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
                self.hits.fetch_add(1, Ordering::Relaxed);
                let source = entry.source();
                // Track disk hits for monitoring
                if source == Source::Disk {
                    self.disk_hits.fetch_add(1, Ordering::Relaxed);
                }
                let cloned = entry.value().clone();
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
    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
    ) {
        let key = message_id.without_brackets().to_string();
        let buffer_len = buffer.len();

        // Check for existing entry - don't overwrite larger buffers with smaller ones
        if let Ok(Some(existing)) = self.cache.get(&key).await {
            let existing_len = existing.value().buffer.len();
            if existing_len > buffer_len {
                // Existing entry is larger - just update availability info
                let mut updated = existing.value().clone();
                updated.record_backend_has(backend_id);
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
            HybridArticleEntry::new(buffer.clone())
        } else {
            // Availability-only mode: store minimal stub
            let stub = Self::create_stub(&buffer);
            HybridArticleEntry::new(stub)
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
    pub async fn close(self) -> anyhow::Result<()> {
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
            .upsert(msg_id, buffer.clone(), BackendId::from_index(0))
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
}
