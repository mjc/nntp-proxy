# Backend Availability Tracking Design

**Purpose**: Track which backends have which articles to avoid sending `430 No such article` responses to clients when another backend has the article.

**Approach**: Unified cache - modify existing `ArticleCache` to store structured `Article` data with backend availability bitset instead of raw response bytes.

**Status**: Design phase - DO NOT COMMIT

---

## Current System Analysis

### 1. Backend Architecture

**Backend Representation** (`src/types.rs:74-90`):
```rust
pub struct BackendId(usize);  // Zero-cost wrapper around index
```

- Backends identified by 0-based index (BackendId::from_index(0), BackendId::from_index(1), etc.)
- Typical setup: 2 backends (from `config.toml` - usenet.farm + NewsDemon)
- Max realistic: ~8 backends (router pre-allocates for 4, `src/router/mod.rs:147`)

**Backend Storage** (`src/router/mod.rs:151-189`):
```rust
self.backends: Vec<BackendInfo>
```

- Backends stored in Vec, indexed by BackendId
- Each backend has: id, name, provider, pending_count, stateful_count

### 2. Article Caching System

**Current Article Cache** (`src/cache/article.rs:26-71`):
```rust
pub struct ArticleCache {
    cache: moka::future::Cache<Arc<str>, CachedArticle>,
}

pub struct CachedArticle {
    pub response: Arc<Vec<u8>>,  // Complete NNTP response (220 + headers + body + terminator)
}
```

**Key Points**:
- Keys: `Arc<str>` - message ID WITHOUT brackets (e.g., `test@example.com` not `<test@example.com>`)
- Values: Complete article response buffered in memory
- Zero-allocation lookups via `Borrow<str>` trait (`src/cache/article.rs:73-89`)
- LRU eviction with TTL
- Used in `nntp-cache-proxy` binary only (not main `nntp-proxy`)

### 3. Message ID Type

**MessageId** (`src/types/protocol.rs:12-93`):
```rust
pub struct MessageId<'a> {
    raw: Cow<'a, str>,  // Includes angle brackets: <msgid@domain>
}
```

**Key Methods**:
- `without_brackets() -> &str` - Returns inner part for cache key
- `extract_from_command(cmd: &str) -> Option<&str>` - Fast extraction from command line
- `from_borrowed(s: &str) -> Result<Self>` - Validates format (must have `<` and `@`)
- Implements `Hash` and `Eq` for use as HashMap/Cache key

### 4. Command Classification

**Article Commands** (`src/command/classifier.rs:418-443`):
```rust
pub enum NntpCommand {
    ArticleByMessageId,  // ARTICLE/BODY/HEAD/STAT <msgid> (70%+ of traffic)
    Stateful,            // ARTICLE/BODY/HEAD/STAT by number, GROUP, NEXT, LAST, XOVER
    // ... other variants
}
```

**Fast Detection** (`src/command/classifier.rs:334-383`):
```rust
fn is_article_cmd_with_msgid(bytes: &[u8]) -> bool {
    // ULTRA-FAST: Direct byte comparison
    // Checks for "ARTICLE <", "BODY <", "HEAD <", "STAT <"
    // 4-6ns hot path (70%+ of traffic)
}
```

**Key Commands for Our Design**:
- `ARTICLE <msgid>` - Retrieve full article (220 response, multiline)
- `BODY <msgid>` - Retrieve article body only (222 response, multiline)
- `HEAD <msgid>` - Retrieve headers only (221 response, multiline)
- `STAT <msgid>` - Check article existence (223 response, **single-line**)

### 5. Response Codes

**Response Parsing** (`src/protocol/response.rs:130-165`):
```rust
pub enum NntpResponse {
    MultilineData(StatusCode),  // 220, 221, 222, etc.
    SingleLine(StatusCode),     // 223, 430, etc.
    // ...
}

impl StatusCode {
    pub fn is_success(&self) -> bool {
        (200..400).contains(&code)  // 2xx and 3xx
    }
    pub fn is_error(&self) -> bool {
        (400..600).contains(&code)  // 4xx and 5xx
    }
}
```

**Relevant Response Codes** (from RFC 3977):
- **220**: Article retrieved (ARTICLE command) - multiline
- **221**: Head retrieved (HEAD command) - multiline
- **222**: Body retrieved (BODY command) - multiline
- **223**: Article exists (STAT command) - **single-line**
- **430**: No such article - single-line

### 6. Current Command Execution Flow (TO BE REPLACED)

**Per-Command Mode** (`src/session/handlers/per_command.rs:270-346`):

**PROBLEM**: Dual code paths with separate cache vs non-cache logic:

```rust
async fn route_and_execute_command(...) -> Result<()> {
    // 1. Check cache for ARTICLE by message-ID
    if let Some(backend_id) = self.check_cache_and_serve(...).await? {
        // Cache hit - already served to client
        return Ok(());
    }
    
    // 2. Route to backend (round-robin)
    let backend_id = router.route_command(self.client_id, command)?;
    
    // 3. Get pooled connection
    let mut pooled_conn = provider.get_pooled_connection().await?;
    
    // 4. Execute command - DUAL CODE PATHS (BAD!)
    if self.should_cache_command(command) {
        // Buffer entire response for caching
        self.execute_command_with_caching(...).await;  // ← PATH 1
    } else {
        // Stream response directly (zero-copy)
        self.execute_command_on_backend(...).await;    // ← PATH 2
    }
    
    // 5. Return connection to pool
    // (automatic via Drop)
}
```

**Cache Check** (`src/session/handlers/per_command.rs:702-762`):
```rust
async fn check_cache_and_serve(...) -> Result<Option<BackendId>> {
    // Only check cache for ARTICLE by message-ID
    let Some((cache, message_id)) = self.cache.as_ref()
        .filter(|_| matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId))
        .zip(extract_message_id(command))
        .and_then(|(cache, msg_id_str)| {
            MessageId::from_borrowed(msg_id_str).ok().map(|msg_id| (cache, msg_id))
        })
    else {
        return Ok(None);  // Cache miss
    };
    
    // Try to get from cache
    match cache.get(&message_id).await {
        Some(cached) => {
            // Serve cached response to client
            client_write.write_all(&cached.response).await?;
            Ok(Some(backend_id))  // Return backend for metrics
        }
        None => Ok(None)  // Cache miss
    }
}
```

**Execute with Caching** (`src/session/handlers/per_command.rs:560-700`) - **TO BE REMOVED**:
```rust
// This function will be DELETED - all commands will use unified cache logic
async fn execute_command_with_caching(...) -> (...) {
    let mut response_buffer = Vec::new();
    
    // 1. Send command to backend
    let (n, response_code, is_multiline, ...) = 
        backend::send_command_and_read_first_chunk(...).await?;
    
    // 2. Buffer first chunk
    response_buffer.extend_from_slice(&chunk_buffer[..n]);
    
    // 3. For multiline, read all chunks
    if is_multiline {
        loop {
            let bytes_read = pooled_conn.read(chunk_buffer).await?;
            response_buffer.extend_from_slice(&chunk_buffer[..bytes_read]);
            
            if has_terminator_at_end(chunk) { break; }
        }
    }
    
    // 4. Write buffered response to client
    client_write.write_all(&response_buffer).await?;
    
    // 5. Cache successful ARTICLE responses (2xx codes)
    if let Some(cache) = &self.cache
        && let Some(message_id_str) = extract_message_id(command)
        && let Ok(message_id) = MessageId::from_borrowed(message_id_str)
        && let Some(code) = response_code.status_code()
        && code.is_success()
    {
        cache.insert(message_id, CachedArticle {
            response: Arc::new(response_buffer),
        }).await;
    }
    
    // Return metrics
}
```

**Execute without Caching** (`src/session/handlers/per_command.rs:408-548`) - **TO BE REMOVED**:
```rust
// This function will be DELETED - all commands will use unified cache logic
async fn execute_command_on_backend(...) -> (...) {
    // 1. Send command and read first chunk
    let (n, response_code, is_multiline, ...) = 
        backend::send_command_and_read_first_chunk(...).await?;
    
    // 2. Stream to client (zero-copy for multiline)
    let bytes_written = if is_multiline {
        stream_multiline_response(pooled_conn, client_write, &chunk_buffer[..n], n, ...).await?
    } else {
        // Single-line - write first chunk only
        client_write.write_all(&chunk_buffer[..n]).await?;
        n as u64
    };
    
    // 3. Track metrics for error codes
    if let Some(code) = response_code.status_code() {
        let raw_code = code.as_u16();
        
        // Track 4xx errors (excluding 423, 430 - normal "not found")
        if (400..500).contains(&raw_code) && raw_code != 423 && raw_code != 430 {
            metrics.record_error_4xx(backend_id);
        }
    }
    
    // No caching - response already streamed to client
}
```

**Backend Communication** (`src/session/backend.rs:89-163`):
```rust
pub async fn send_command_and_read_first_chunk<T>(...) 
    -> Result<(usize, NntpResponse, bool, u64, u64, u64)>
{
    // 1. Send command
    backend_conn.write_all(command.as_bytes()).await?;
    
    // 2. Read first chunk
    let n = chunk.read_from(backend_conn).await?;
    
    // 3. Validate and parse response
    let validated = validate_backend_response(&chunk[..n], n, MIN_RESPONSE_LENGTH);
    
    // Returns: (bytes_read, response_code, is_multiline, ttfb, send_time, recv_time)
    Ok((n, validated.response, validated.is_multiline, ...))
}
```

---

## Problem Statement

**Current Behavior**:
1. Client requests `ARTICLE <msgid@domain>`
2. Proxy routes to Backend A (round-robin)
3. Backend A returns `430 No such article`
4. Proxy forwards `430` to client
5. **Article might exist on Backend B**, but client gives up

**User Frustration**:
> "getting a lot of these now... If no backend has the article then it should return 430 to the client [but if another backend has it, try that one]"

**Why This Matters**:
- Usenet articles are distributed across providers
- Not all providers have all articles (retention, propagation, DMCA)
- Client (SABnzbd/NZBGet) only tries once per message-ID
- Missing articles fail downloads even when another backend has them

---

## Requirements

1. **Track backend availability per message-ID** - Which backends have/don't have each article
2. **STAT/HEAD update cache** - Lightweight commands should populate availability data
3. **BODY/ARTICLE build bitset on miss** - If cache doesn't know, try backends and learn
4. **Update cache AFTER response sent** - Don't block client on cache write
5. **Handle three scenarios**:
   - First backend has article → serve immediately
   - First backend doesn't have it, another does → retry and serve
   - No backend has it → return 430 to client

---

## Proposed Design

### Data Structure: Unified Article Cache

**Strategy**: Store `Arc<Vec<u8>>` of the response buffer + parsed offsets + backend availability bitset.

**Key Insight**: 
- We already parse responses with `protocol::Article::parse()` for validation
- `protocol::Article<'a>` borrows from buffer (lifetime `'a`) - can't store directly
- **Instead of cloning data**: Arc the buffer, store offsets to headers/body within buffer
- `Article<'a>` can be reconstructed on-demand by parsing the Arc'd buffer
- **On insert**: Take buffer ownership via Arc, store metadata + bitset
- **On serve**: Parse Article from Arc'd buffer (zero-allocation), serialize to client

**Problem**: `protocol::Article<'a>` borrows from buffer (lifetime `'a`) - can't store directly.

**Smart Solution**: Reuse `protocol::Article` parsing, just Arc the buffer:

```rust
// In src/cache/article.rs

use crate::protocol::{Article, Headers};

/// Cached article - owned version of protocol::Article
/// 
/// Stores Arc'd buffer and reuses protocol::Article parsing logic.
/// No duplication - just wraps the borrowed Article with ownership.
pub struct CachedArticle {
    /// Backend availability bitset (2 bytes)
    backend_availability: BackendAvailability,
    
    /// Complete response buffer (Arc for cheap cloning)
    buffer: Arc<Vec<u8>>,
    
    /// Status code (220/221/222/223) - cheap to store
    status_code: u16,
}

impl CachedArticle {
    /// Create from parsed article + backend ID
    /// 
    /// IMPORTANT: Buffer MUST be validated before calling this.
    /// We validate once here, then use unchecked accessors forever.
    pub fn from_parsed(
        buffer: Vec<u8>,
        status_code: u16,
        backend_id: BackendId,
    ) -> Result<Self, ParseError> {
        // Validate ONCE on insert (with yenc validation)
        let _ = Article::parse(&buffer, true)?;
        
        let mut availability = BackendAvailability::empty();
        availability.mark_available(backend_id);
        
        Ok(Self {
            backend_availability: availability,
            buffer: Arc::new(buffer),
            status_code,
        })
    }
    
    /// Parse the buffer as Article (validated once on insert)
    /// 
    /// SAFETY: Buffer was validated during from_parsed(), so we use unsafe
    /// parsing path that skips re-validation. This is safe because:
    /// 1. Buffer is immutable (Arc<Vec<u8>>)
    /// 2. Already validated when inserted into cache
    /// 3. No way to modify buffer after creation
    #[inline]
    pub fn as_article_unchecked(&self) -> Article<'_> {
        // TODO: Add Article::parse_unchecked() that skips validation
        // For now, use regular parse (will add unchecked later)
        Article::parse(&self.buffer, false)
            .expect("Cached article buffer invalid (already validated on insert)")
    }
    
    /// Get message ID (zero-copy, no validation)
    #[inline]
    pub fn message_id(&self) -> MessageId<'_> {
        self.as_article_unchecked().message_id
    }
    
    /// Get headers if available (zero-copy, no validation)
    #[inline]
    pub fn headers(&self) -> Option<Headers<'_>> {
        self.as_article_unchecked().headers
    }
    
    /// Get specific header by name (zero-copy, no validation)
    pub fn header(&self, name: &str) -> Option<&str> {
        let article = self.as_article_unchecked();
        if let Some(headers) = article.headers {
            for (hname, hvalue) in headers.iter() {
                if hname.eq_ignore_ascii_case(name.as_bytes()) {
                    // SAFETY: headers already validated on insert
                    return Some(unsafe { std::str::from_utf8_unchecked(hvalue) });
                }
            }
        }
        None
    }
    
    /// Get body if available (zero-copy, no validation)
    #[inline]
    pub fn body(&self) -> Option<&[u8]> {
        self.as_article_unchecked().body
    }
    
    /// Get article number (zero-copy, no validation)
    #[inline]
    pub fn article_number(&self) -> Option<u64> {
        self.as_article_unchecked().article_number
    }
    
    /// Get raw buffer for serving to client
    pub fn buffer(&self) -> &Arc<Vec<u8>> {
        &self.buffer
    }
    
    /// Get status code
    pub fn status_code(&self) -> u16 {
        self.status_code
    }
}

impl CachedArticle {
    /// Create from already-parsed protocol::Article
    /// 
    /// Wraps the headers and body slices in Arc'd owned types.
    pub fn from_parsed(
        backend_id: BackendId,
        article: &crate::protocol::Article<'_>,
        status_code: u16,
    ) -> Self {
        let mut availability = BackendAvailability::new();
        availability.mark_has(backend_id);
        
        Self {
            backend_availability: availability,
            message_id: Arc::from(article.message_id.without_brackets()),
            article_number: article.article_number,
            status_code,
            // Wrap in owned types with Arc'd data
            headers: article.headers.map(OwnedHeaders::from_headers),
            body: article.body.map(OwnedBody::from_slice),
        }
    }
    
    /// Create availability-only entry (cache disabled mode)
    /// 
    /// Only tracks which backend has the article, doesn't store response.
    /// Used when `--cache-capacity 0`.
    pub fn availability_only(backend_id: BackendId, message_id: &str) -> Self {
        let mut availability = BackendAvailability::new();
        availability.mark_has(backend_id);
        
        Self {
            backend_availability: availability,
            message_id: Arc::from(message_id),
            article_number: None,
            status_code: 223,  // STAT-equivalent
            headers: None,
            body: None,
        }
    }
    
    
    /// Update with new data from backend (progressive enhancement)
    /// 
    /// Merges new parsed article data into existing cache entry:
    /// - STAT (223) → HEAD (221): Add headers
    /// - STAT (223) → BODY (222): Add body
    /// - STAT (223) → ARTICLE (220): Add headers + body
    /// - HEAD (221) → ARTICLE (220): Add body
    /// - BODY (222) → ARTICLE (220): Add headers
    /// 
    /// Each piece is Arc'd independently, so multiple backends can contribute different parts.
    pub fn update_from_parsed(
        &mut self,
        backend_id: BackendId,
        article: &crate::protocol::Article<'_>,
        status_code: u16,
    ) {
        // Mark this backend as having the article
        self.backend_availability.mark_has(backend_id);
        
        // Update status code to the most complete type we've seen
        // Priority: 220 (ARTICLE) > 221 (HEAD) = 222 (BODY) > 223 (STAT)
        match (self.status_code, status_code) {
            (223, _) => self.status_code = status_code, // Anything better than STAT
            (221, 220) | (221, 222) => self.status_code = status_code, // Upgrade HEAD
            (222, 220) | (222, 221) => self.status_code = status_code, // Upgrade BODY  
            (220, _) => {}, // Already complete, don't downgrade
            _ => {} // Keep existing
        }
        
        // Merge headers (don't overwrite if we already have them)
        if self.headers.is_none() {
            self.headers = article.headers.map(OwnedHeaders::from_headers);
        }
        
        // Merge body (don't overwrite if we already have it)
        if self.body.is_none() {
            self.body = article.body.map(OwnedBody::from_slice);
        }
        
        // Update article number if we didn't have it
        if self.article_number.is_none() {
            self.article_number = article.article_number;
        }
    }
    
    /// Serialize back to NNTP wire format for serving to client
    pub fn serialize_to(&self, buf: &mut Vec<u8>) -> usize {
        let start_len = buf.len();
        
        // Status line: "220 100 <msgid>\r\n"
        use std::fmt::Write as _;
        let _ = write!(buf, "{} ", self.status_code);
        if let Some(num) = self.article_number {
            let _ = write!(buf, "{} ", num);
        } else {
            buf.extend_from_slice(b"0 ");
        }
        buf.extend_from_slice(b"<");
        buf.extend_from_slice(self.message_id.as_bytes());
        buf.extend_from_slice(b">\r\n");
        
        // Headers if present (HEAD or ARTICLE)
        if let Some(headers) = &self.headers {
            buf.extend_from_slice(headers.as_bytes());
            if !headers.as_bytes().ends_with(b"\r\n") {
                buf.extend_from_slice(b"\r\n");
            }
        }
        
        // Blank line separator if we have both headers and body
        if self.headers.is_some() && self.body.is_some() {
            buf.extend_from_slice(b"\r\n");
        }
        
        // Body if present (BODY or ARTICLE)
        if let Some(body) = &self.body {
            buf.extend_from_slice(body.as_bytes());
            if !body.as_bytes().ends_with(b"\r\n") {
                buf.extend_from_slice(b"\r\n");
            }
        }
        
        // Terminator
        buf.extend_from_slice(b".\r\n");
        
        buf.len() - start_len
    }
    
    /// Get backends that might have this article
    pub fn potential_backends(&self, total_backends: usize) -> Vec<BackendId> {
        self.backend_availability.potential_backends(total_backends)
    }
    
    /// Mark backend as not having this article (430 response)
    pub fn mark_backend_missing(&mut self, backend_id: BackendId) {
        self.backend_availability.mark_missing(backend_id);
    }
    
    /// Update with new data from backend (progressive enhancement)
    /// 
    /// Merges new parsed article data into existing cache entry:
    /// - STAT (223) → HEAD (221): Add headers
    /// - STAT (223) → BODY (222): Add body
    /// - STAT (223) → ARTICLE (220): Add headers + body
    /// - HEAD (221) → ARTICLE (220): Add body
    /// - BODY (222) → ARTICLE (220): Add headers
    /// 
    /// **Always marks backend as having the article** (positive confirmation).
    /// 
    /// **Note on reliability**:
    /// - ARTICLE/BODY success (220/222): Completely reliable - article definitely exists
    /// - STAT/HEAD success (223/221): Potentially unreliable - metadata exists but article might not
    /// - Caller should be aware STAT/HEAD might need validation via later ARTICLE request
    pub fn update_from_parsed(
        &mut self,
        backend_id: BackendId,
        article: &crate::protocol::Article<'_>,
        status_code: u16,
    ) {
        // Mark this backend as having the article
        self.backend_availability.mark_has(backend_id);
        
        // Update status code to the most complete type we've seen
        // Priority: 220 (ARTICLE) > 221 (HEAD) = 222 (BODY) > 223 (STAT)
        match (self.status_code, status_code) {
            (223, _) => self.status_code = status_code, // Anything better than STAT
            (221, 220) | (221, 222) => self.status_code = status_code, // Upgrade HEAD
            (222, 220) | (222, 221) => self.status_code = status_code, // Upgrade BODY
            _ => {} // Keep existing if already complete or new isn't better
        }
        
        // Merge headers (don't overwrite if we already have them)
        if self.headers.is_none() {
            if let Some(headers) = article.headers {
                self.headers = Some(Arc::new(headers.as_bytes().to_vec()));
            }
        }
        
        // Merge body (don't overwrite if we already have it)
        if self.body.is_none() {
            if let Some(body) = article.body {
                self.body = Some(Arc::new(body.to_vec()));
            }
        }
        
        // Update article number if we didn't have it
        if self.article_number.is_none() {
            self.article_number = article.article_number;
        }
    }
    
    /// Check if we have response data to serve
    /// False for STAT-only entries
    pub fn has_data(&self) -> bool {
        self.headers.is_some() || self.body.is_some()
    }
    
    /// Check if this is a complete ARTICLE (has both headers and body)
    pub fn is_complete(&self) -> bool {
        self.headers.is_some() && self.body.is_some()
    }
}
```

**Different response types** (based on which fields are Some):
- **STAT (223)**: `headers: None, body: None` - just availability tracking
- **HEAD (221)**: `headers: Some(OwnedHeaders), body: None` - headers only
- **BODY (222)**: `headers: None, body: Some(OwnedBody)` - body only  
- **ARTICLE (220)**: `headers: Some(OwnedHeaders), body: Some(OwnedBody)` - complete article

**On INSERT**:
```rust
// We already parsed for validation
let parsed = protocol::Article::parse(&response_buf, validate_yenc)?;

// Arc the slices and cache - buffer can be dropped after this
let cached = CachedArticle::from_parsed(backend_id, &parsed, 220);
cache.insert(message_id, cached).await;
// response_buf goes out of scope, but Arc keeps headers/body alive
```

**On SERVE**:
```rust
let cached = cache.get(&message_id).await?;
let mut response_buf = Vec::new();  // Get from buffer pool
cached.serialize_to(&mut response_buf);  // Reconstruct NNTP response
client_write.write_all(&response_buf).await?;
```

**Allocations**:
- **On insert**: Arc::from() headers slice, Arc::from() body slice (2 allocations for ARTICLE, 1 for HEAD/BODY, 0 for STAT)
- **On serve**: Reconstruct response into buffer (copies Arc'd data)

```rust
/// Bitset representing which backends have an article
/// 
/// Bit N is set if BackendId::from_index(N) has the article.
/// Bit N is clear if BackendId::from_index(N) returned 430.
/// 
/// Examples (2 backends):
/// - 0b11 = Both backends have article
/// - 0b10 = Backend 1 has it, Backend 0 doesn't
/// - 0b01 = Backend 0 has it, Backend 1 doesn't
/// - 0b00 = No backend has it (or not yet checked)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendAvailability {
    /// Bitset of backends that have this article
    has_article: u8,  // u8 supports up to 8 backends (plenty for NNTP)
    
    /// Bitset of backends we've checked (to distinguish "not checked" from "doesn't have")
    checked: u8,
}

impl BackendAvailability {
    /// Create empty availability (no backends checked)
    pub const fn new() -> Self {
        Self {
            has_article: 0,
            checked: 0,
        }
    }
    
    /// Mark backend as having the article (success response: 220, 221, 222, 223)
    pub fn mark_has(&mut self, backend_id: BackendId) {
        let bit = 1u8 << backend_id.as_index();
        self.has_article |= bit;
        self.checked |= bit;
    }
    
    /// Mark backend as NOT having the article (430 response)
    /// 
    /// **CRITICAL**: Call this for ALL 430 responses - they are authoritative.
    /// 
    /// 430 responses are reliable:
    /// - STAT/HEAD 430: Metadata doesn't exist → article almost certainly doesn't exist
    /// - ARTICLE/BODY 430: Article storage doesn't have it → definitely doesn't exist
    /// - Both are trustworthy for marking as missing
    /// 
    /// **Contrast with success responses**:
    /// - STAT/HEAD success (223/221): Might be false positive (metadata exists, article doesn't)
    /// - ARTICLE/BODY success (220/222): Always accurate (article definitely exists)
    pub fn mark_missing(&mut self, backend_id: BackendId) {
        let bit = 1u8 << backend_id.as_index();
        self.has_article &= !bit;  // Clear bit
        self.checked |= bit;       // Mark as checked
    }
    
    /// Check if backend has the article
    pub fn has(&self, backend_id: BackendId) -> Option<bool> {
        let bit = 1u8 << backend_id.as_index();
        if self.checked & bit == 0 {
            None  // Not yet checked
        } else {
            Some(self.has_article & bit != 0)
        }
    }
    
    /// Get list of backends that might have the article
    /// (includes backends we haven't checked yet)
    pub fn potential_backends(&self, total_backends: usize) -> Vec<BackendId> {
        let mut result = Vec::new();
        for i in 0..total_backends {
            let backend_id = BackendId::from_index(i);
            match self.has(backend_id) {
                Some(true) => result.push(backend_id),  // Known to have
                None => result.push(backend_id),        // Not yet checked
                Some(false) => {},                      // Known to NOT have
            }
        }
        result
    }
    
    /// Check if all backends have been checked and all returned 430
    pub fn all_backends_missing(&self, total_backends: usize) -> bool {
        let all_checked_mask = (1u8 << total_backends) - 1;
        self.checked == all_checked_mask && self.has_article == 0
    }
}
```

### OwnedArticle Structure

**Note**: See the OwnedArticle design above for the actual cache storage structure.
This section shows the ArticleCache interface updates.

```rust
/// Article cache using LRU eviction with TTL
/// Now stores OwnedArticle (protocol::Article data + bitset) instead of raw responses
#[derive(Clone)]
pub struct ArticleCache {
    cache: Arc<Cache<Arc<str>, OwnedArticle>>,
}

impl ArticleCache {
    // Existing methods updated to use OwnedArticle:
    // - get() now returns Option<OwnedArticle>
    // - stats(), sync() remain unchanged
    
    /// Upsert cache entry from parsed article (insert or update)
    /// 
    /// If entry exists: merges new data (STAT → HEAD → BODY → ARTICLE)
    /// If entry doesn't exist: inserts new entry
    /// 
    /// **Storage behavior depends on cache capacity**:
    /// - If capacity = 0: Store availability only (headers/body = None)
    /// - If capacity > 0: Store full article (headers/body = Some(...))
    /// 
    /// This handles ALL cache updates - capacity determines what gets stored.
    pub async fn upsert(
        &self,
        message_id: &MessageId<'_>,
        backend_id: BackendId,
        article: &crate::protocol::Article<'_>,
        status_code: u16,
    ) {
        let key = Arc::from(message_id.without_brackets());
        
        // Check if we should store full article data or just availability
        let cache_enabled = self.cache.max_capacity().unwrap_or(0) > 0;
        
        if let Some(mut existing) = self.cache.get(&*key).await {
            // Update existing entry (progressive enhancement)
            existing.update_from_parsed(backend_id, article, status_code, cache_enabled);
            self.cache.insert(key, existing).await;
        } else {
            // Create new entry
            let owned = if cache_enabled {
                // Full cache: store headers + body
                OwnedArticle::from_parsed(backend_id, article, status_code)
            } else {
                // Availability-only: just bitset + metadata
                OwnedArticle::availability_only(backend_id, article.message_id.as_str())
            };
            self.cache.insert(key, owned).await;
        }
    }
    
    /// Mark backend as not having this article (430 response)
    pub async fn mark_backend_missing(&self, message_id: &MessageId<'_>, backend_id: BackendId) {
        let key = Arc::from(message_id.without_brackets());
        
        if let Some(mut article) = self.cache.get(&*key).await {
            article.mark_backend_missing(backend_id);
            self.cache.insert(key, article).await;
        }
    }
}
```

**Delete `CachedArticle` type** - replaced by richer `OwnedArticle`

---

## Critical: Handling STAT/HEAD False Positives

### The Problem

**False positives are common with STAT/HEAD success responses**:
- STAT/HEAD check metadata indexes, not actual article storage
- Backend might return 223 (STAT success) but later return 430 for ARTICLE
- Metadata exists but article body was purged/incomplete
- XOVER data present but article itself is gone
- Backend says "I have it" but ARTICLE request fails

**False negatives are rare with STAT/HEAD**:
- If STAT/HEAD returns 430, article almost certainly doesn't exist
- Metadata missing usually means article missing
- 430 from STAT/HEAD is reliable

**ARTICLE/BODY responses are authoritative**:
- Both 430 (doesn't exist) and success (exists) are completely reliable
- These commands access actual article storage, not indexes
- No false positives or false negatives

### The Solution

**Trust 430 from any command, but only trust success from ARTICLE/BODY**:

```rust
if status_code == 430 {
    // ALL 430 responses are authoritative - article doesn't exist
    cache.mark_backend_missing(message_id, backend_id).await;
} else if status_code.is_success() {
    match command_type {
        NntpCommand::ArticleByMessageId | NntpCommand::BodyByMessageId => {
            // AUTHORITATIVE success - article definitely exists
            cache.upsert(message_id, backend_id, &parsed, status_code).await;
        }
        NntpCommand::StatByMessageId | NntpCommand::HeadByMessageId => {
            // POTENTIALLY FALSE POSITIVE - metadata exists but article might not
            // Only cache if cache is enabled (store metadata for faster subsequent requests)
            // But don't fully trust the availability - mark as "possibly has"
            if cache_enabled {
                cache.upsert(message_id, backend_id, &parsed, status_code).await;
            }
        }
    }
}
```

### Impact on Smart Routing

**Scenario**: Backend A returns STAT 223, but later ARTICLE returns 430

**Problem with naive approach**:
1. STAT 223 from Backend A → Mark as "has article" (wrong!)
2. Future ARTICLE requests go to Backend A
3. Backend A returns 430 (metadata existed but article didn't)
4. Need to retry on Backend B

**Better approach (current design)**:
1. STAT 223 from Backend A → Cache metadata but don't over-trust availability
2. ARTICLE 430 from Backend A → NOW mark as definitely doesn't have
3. Future ARTICLE requests avoid Backend A

**Even better approach (future enhancement)**:
1. Track "confirmed via ARTICLE" vs "only seen via STAT" separately
2. Prefer backends confirmed via ARTICLE
3. Use STAT-only confirmations as hints, not guarantees

**Current conservative rule**:
- **Trust all 430 responses** (mark backend as missing)
- **Trust ARTICLE/BODY success** (mark backend as having)
- **Cache but be skeptical of STAT/HEAD success** (store data but may need retry)

---

## Key Design Advantages

### 1. **Single Source of Truth**
- Backend availability lives with article data
- No risk of cache inconsistency between two separate caches
- Simpler mental model: one cache, multiple use cases

### 2. **Progressive Enhancement**
- STAT (223) → Cache availability only (headers: None, body: None)
- HEAD (221) → Add headers (headers: Some(Arc<Vec<u8>>), body: None)
- BODY (222) → Add body (headers: None, body: Some(Arc<Vec<u8>>))
- ARTICLE (220) → Complete (headers: Some(...), body: Some(...))
- Each response type can be cached independently or upgrade existing entry

### 3. **Memory Efficiency**
- Only store what we have: STAT responses are minimal (~68 bytes overhead)
- Headers and body stored as Arc<Vec<u8>> for cheap cloning
- Backend availability adds just 2 bytes per article
- Different response types have different memory footprints

### 4. **Reuses Existing Configuration**
- Reuse existing `--cache-capacity` and `--cache-ttl` flags
- Reuse existing `CacheConfig` in config.toml
- No new command-line arguments or config sections
- **Note**: Behavior changes even with cache disabled (now does smart routing)

### 5. **Code Simplification**
- Delete `CachedArticle` struct (replaced by richer `OwnedArticle`)
- No separate availability cache to manage
- Existing cache infrastructure (Moka, Arc<str> keys, TTL) handles everything

### 6. **Smart Caching Strategy**
- **Backend availability is ALWAYS tracked** (even with `--cache-capacity 0`)
- **Cache disabled** (`--cache-capacity 0`):
  - Tracks availability (2 bytes per article)
  - Smart routing to backends that have the article
  - 430 retry logic across backends
  - **Does NOT store** headers or body (saves memory)
  - **This is a BEHAVIOR CHANGE** - not backwards compatible
- **Cache enabled** (`--cache-capacity > 0`):
  - Everything from disabled mode PLUS
  - Stores full articles (headers + body)
  - Can serve from cache without backend query
- **Rationale**: Availability tracking is lightweight and dramatically reduces 430 errors
- **No separate code paths**: Same logic handles both modes, just different storage
- **Migration impact**: Even users with cache disabled will get smart routing behavior

---

### Integration Points

#### 1. Proxy Builder (`src/proxy.rs`)

**Change required**: Remove `Option<>` wrapper from cache field:

```rust
// OLD (cache is optional)
pub struct NntpProxy {
    cache: Option<Arc<ArticleCache>>,  // ← Optional, can be None
    // ...
}

// NEW (cache is always present)
pub struct NntpProxy {
    cache: Arc<ArticleCache>,  // ← Always present, just configured differently
    // ...
}
```

**Rationale**: Cache is always needed for availability tracking. The `--cache-capacity` setting determines what gets stored (availability-only vs full articles), not whether cache exists.

#### 2. Session (`src/session/handlers/per_command.rs`)

**Change required**: Remove `Option<>` wrapper and conditional cache checks:

```rust
// OLD (cache might not exist)
pub struct PerCommandSession {
    cache: Option<Arc<ArticleCache>>,  // ← Optional
    // ...
}

// NEW (cache always exists)
pub struct PerCommandSession {
    cache: Arc<ArticleCache>,  // ← Required field
    // ...
}
```

**Impact**: All `if let Some(cache) = &self.cache` checks become direct `self.cache` access.

#### 3. Unified Command Execution Flow (Single Code Path)

**Delete these functions entirely**:
- `execute_command_with_caching()` - No longer needed
- `execute_command_on_backend()` - No longer needed  
- `should_cache_command()` - No longer needed (always cache)

**Updated `check_cache_and_serve()`** - Check cache but don't serve yet:

```rust
async fn check_cache_and_serve(...) -> Result<Option<OwnedArticle>> {
    // Parse command to get message-ID
    let Some((cache, message_id)) = self.cache.as_ref()
        .filter(|_| matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId))
        .zip(extract_message_id(command))
        .and_then(|(cache, msg_id_str)| {
            MessageId::from_borrowed(msg_id_str).ok().map(|msg_id| (cache, msg_id))
        })
    else {
        return Ok(None);
    };
    
    // Check cache - return article if found (even if incomplete)
    if let Some(article) = cache.get(&message_id).await {
        return Ok(Some(article));
    }
    
    Ok(None) // Cache miss
}
```

**New Flow** (`src/session/handlers/per_command.rs:route_and_execute_command`):

```rust
async fn route_and_execute_command(...) -> Result<()> {
    // 1. Check article cache for availability info
    let cache_result = self.check_cache_and_serve(...).await?;
    
    // 2. Determine if we need to query backend
    let (preferred_backends, serve_from_cache) = if let Some(cached_article) = cache_result {
        // Cache hit
        if cached_article.is_complete() {
            // Have complete article (headers + body) - can serve from cache
            (None, Some(cached_article))
        } else {
            // Partial hit (STAT-only or HEAD-only) - need to query backend for missing data
            // Use availability info for smart routing
            (Some(cached_article.potential_backends(router.backend_count())), None)
        }
    } else {
        // Cache miss - try all backends
        (None, None)
    };
    
    // 3. If we can serve from cache, do it now and return
    if let Some(article) = serve_from_cache {
        let mut response_buf = Vec::new(); // TODO: Get from buffer pool
        let bytes_written = article.serialize_to(&mut response_buf);
        client_write.write_all(&response_buf).await?;
        info!("Cache HIT: served {} bytes", bytes_written);
        return Ok(());
    }
    
    // 4. Need to query backend - determine which backends to try
    let backends_to_try = preferred_backends.unwrap_or_else(|| {
        // No cache hints - try all backends
        (0..router.backend_count())
            .map(BackendId::from_index)
            .collect()
    });
    
    // 5. Try each backend until success or exhausted
    let message_id = extract_message_id_if_article_command(command);
    let mut last_error = None;
    
    for (attempt, backend_id) in backends_to_try.iter().enumerate() {
        let provider = router.get_provider(*backend_id)?;
        let mut pooled_conn = provider.get_pooled_connection().await?;
        
        // Execute command and read response
        let (n, status_code, is_multiline, response_buf) = 
            backend::send_command_and_read_first_chunk(...).await?;
        
        // Parse response for cache update
        let parsed_article = if status_code.is_success() {
            protocol::Article::parse(&response_buf, validate_yenc).ok()
        } else {
            None
        };
        
        // Update cache asynchronously (don't block client)
        if let (Some(cache), Some(msg_id)) = (&self.cache, message_id) {
            if let Ok(parsed_msgid) = MessageId::from_borrowed(msg_id) {
                let backend_id_copy = *backend_id;
                let cache_clone = cache.clone();
                
                tokio::spawn(async move {
                    if status_code.as_u16() == 430 {
                        // ALL 430 responses are authoritative - article doesn't exist
                        cache_clone.mark_backend_missing(&parsed_msgid, backend_id_copy).await;
                    } else if let Some(parsed) = parsed_article {
                        // Success - upsert cache
                        // Note: STAT/HEAD success might be false positive (metadata exists, article doesn't)
                        // but we cache it anyway - if later ARTICLE returns 430, we'll update
                        cache_clone.upsert(
                            &parsed_msgid,
                            backend_id_copy,
                            &parsed,
                            status_code.as_u16(),
                        ).await;
                    }
                });
            }
        }
        
        // Handle response
        if status_code.as_u16() == 430 {
            // Backend doesn't have article
            if attempt < backends_to_try.len() - 1 {
                debug!("Backend {:?} returned 430, trying next backend", backend_id);
                last_error = Some(anyhow::anyhow!("Backend {:?} returned 430", backend_id));
                continue; // Try next backend
            } else {
                // Last backend also returned 430 - send to client
                client_write.write_all(&response_buf[..n]).await?;
                return Ok(());
            }
        }
        
        // Success - stream response to client
        client_write.write_all(&response_buf[..n]).await?;
        if is_multiline {
            stream_multiline_response(client_write, backend_read, buffer).await?;
        }
        
        return Ok(());
    }
    
    // All backends exhausted without success
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All backends failed")))
}
```

#### 4. STAT/HEAD/BODY Special Handling

**Different response types** are handled automatically by `OwnedArticle::from_parsed()`:

- **STAT (223)**: headers: None, body: None - Just availability tracking (~68 bytes)
- **HEAD (221)**: headers: Some(...), body: None - Headers only (~5KB typical)
- **BODY (222)**: headers: None, body: Some(...) - Body only (~45KB typical)
- **ARTICLE (220)**: headers: Some(...), body: Some(...) - Complete (~50KB typical)

**Progressive Enhancement Example**:
```rust
// Client sends STAT first
STAT <article@example.com>
  → Backend 0 returns 223
  → Parse: Article { message_id, article_number, headers: None, body: None }
  → Cache upsert (new entry):
     OwnedArticle { availability: [1,0], headers: None, body: None, status: 223 }

// Later, client sends HEAD
HEAD <article@example.com>
  → Cache HIT (STAT entry found)
  → article.has_data() == false, so query backend
  → Try Backend 0 (from potential_backends)
  → Backend 0 returns 221 + headers
  → Parse: Article { message_id, article_number, headers: Some(...), body: None }
  → Cache upsert (update existing):
     - Merge headers into existing entry
     - Keep availability: [1,0] (Backend 0 still has it)
     - Upgrade status: 223 → 221
  → Result: OwnedArticle { availability: [1,0], headers: Some(Arc), body: None, status: 221 }

// Later, client sends ARTICLE  
ARTICLE <article@example.com>
  → Cache HIT (HEAD entry found)
  → article.has_data() == true, but article.is_complete() == false
  → Serve headers from cache, but still need body
  → Try Backend 0
  → Backend 0 returns 220 + full article
  → Parse: Article { message_id, article_number, headers: Some(...), body: Some(...) }
  → Cache upsert (update existing):
     - Keep existing headers (already have them)
     - Merge body into entry
     - Upgrade status: 221 → 220
  → Result: OwnedArticle { availability: [1,0], headers: Some(Arc), body: Some(Arc), status: 220 }

// Later, different client requests from Backend 1
ARTICLE <article@example.com>
  → Cache HIT (complete ARTICLE)
  → article.is_complete() == true
  → Serve from cache (both headers and body)
  → No backend query needed

// But if we get a request that goes to Backend 1 for some reason
HEAD <article@example.com> (routed to Backend 1)
  → Backend 1 returns 221
  → Cache upsert (update existing):
     - Headers already exist (don't overwrite)
     - Body already exists (don't overwrite)
     - UPDATE availability: [1,0] → [1,1] (now both backends have it!)
     - Status stays 220 (already complete)
  → Result: OwnedArticle { availability: [1,1], headers: Some(Arc), body: Some(Arc), status: 220 }
```

**Benefits**:
- Each response type can be cached independently or upgrade existing entry
- Availability tracking improves as more backends are queried
- Headers/body are never duplicated (don't overwrite if already present)
- Clients can probe cheaply with STAT/HEAD before downloading ARTICLE
- **Multi-backend learning**: If Backend 0 serves HEAD and Backend 1 serves BODY, cache learns both have the article

---

## Configuration

**No new configuration needed** - reuse existing article cache config:

```toml
[cache]
# Enable article caching (headers + body storage)
# Availability tracking is ALWAYS enabled regardless of this setting
enabled = true

# Number of articles to cache (default: 10000)
# If set to 0: Only availability is tracked (no headers/body stored)
# If set to >0: Full articles cached + availability
max_capacity = 10_000

# Cache entry time-to-live in seconds (default: 3600 = 1 hour)
ttl = 3600
```

**Existing config in `src/config/types.rs`** already handles this:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    
    #[serde(default = "default_cache_capacity")]
    pub max_capacity: u64,
    
    #[serde(default = "default_cache_ttl")]
    pub ttl: u64,
}
```

**Usage**: Enable caching via `--cache-capacity` and `--cache-ttl` CLI flags, or in config file. The unified cache stores both article content and backend availability automatically.

---

## Performance Characteristics

### Memory Usage

**Article Entry Sizes** (per message-ID):

**Cache Capacity Planning**:
- **Availability-only mode** (cache disabled, `--cache-capacity 0`):
  - Stores only OwnedArticle with no headers/body data
  - Per article: ~68 bytes (struct overhead + bitset)
  - 10K articles: ~680KB
- **Full cache mode** (cache enabled, `--cache-capacity > 0`):
  - **STAT (223)**: ~68 bytes (struct overhead, no data)
  - **HEAD (221)**: ~68 bytes + ~5KB headers = ~5.1KB
  - **BODY (222)**: ~68 bytes + ~45KB body = ~45KB
  - **ARTICLE (220)**: ~68 bytes + ~5KB headers + ~45KB body = ~50KB
  - **Mixed (typical)**: 5K STAT + 3K HEAD + 2K ARTICLE = ~340KB + 15MB + 90MB = ~105MB

**Comparison to Old CachedArticle**:
- Old: Always stored full response (500KB per entry)
- New: Stores structured data (68 bytes to ~50KB depending on response type)
- **7300x more efficient for STAT responses, 100x for HEAD, same for ARTICLE**

**Arc Benefit**:
- Multiple concurrent readers share same data
- Only increment reference count (atomic operation)
- Memory footprint independent of concurrent requests

### Lookup Performance

**Cache Lookup**:
- Moka is lock-free (concurrent HashMap)
- O(1) average case
- ~10-20ns for cache hit (hash + lookup)

**Bitset Operations**:
- `has()`: Single AND + comparison (~1ns)
- `mark_has()`: Single OR (~1ns)
- `potential_backends()`: Loop over N backends (~5-10ns for typical 2-4 backends)

**Total Overhead per Request**:
- Cache lookup: ~20ns
- Bitset check: ~5ns
- **Total: ~25ns overhead** (negligible compared to 4-6ns command classification)

### Network Impact

**430 Retry Scenario**:
- Without cache: Client gets 430, gives up
- With cache: 
  - First request: Try backend A (430), try backend B (220) → 2x RTT
  - Subsequent requests: Go directly to backend B → 1x RTT
  - **Amortized: same RTT as without cache, but higher success rate**

**STAT Precheck Strategy** (optional future enhancement):
- Client could send `STAT <msgid>` before `ARTICLE <msgid>`
- STAT is single-line (fast), warms availability cache
- Subsequent ARTICLE goes directly to correct backend
- **Tradeoff**: Extra RTT for STAT, but avoids failed ARTICLE attempts

---

## Testing Strategy

### Unit Tests

**Bitset Operations** (`tests/test_backend_availability.rs`):
```rust
#[test]
fn test_availability_bitset_basic() {
    let mut avail = BackendAvailability::new();
    let b0 = BackendId::from_index(0);
    let b1 = BackendId::from_index(1);
    
    assert_eq!(avail.has(b0), None);  // Not checked
    
    avail.mark_has(b0);
    assert_eq!(avail.has(b0), Some(true));
    assert_eq!(avail.has(b1), None);  // Still not checked
    
    avail.mark_missing(b1);
    assert_eq!(avail.has(b1), Some(false));
}

#[test]
fn test_potential_backends() {
    let mut avail = BackendAvailability::new();
    avail.mark_has(BackendId::from_index(0));
    avail.mark_missing(BackendId::from_index(1));
    
    let candidates = avail.potential_backends(3);
    assert_eq!(candidates, vec![
        BackendId::from_index(0),  // Known to have
        BackendId::from_index(2),  // Not yet checked
    ]);
}

#[test]
fn test_all_backends_missing() {
    let mut avail = BackendAvailability::new();
    avail.mark_missing(BackendId::from_index(0));
    avail.mark_missing(BackendId::from_index(1));
    
    assert!(avail.all_backends_missing(2));
    assert!(!avail.all_backends_missing(3));  // Backend 2 not checked
}
```

### Integration Tests

**430 Retry Scenario** (`tests/test_backend_availability_integration.rs`):
```rust
#[tokio::test]
async fn test_430_retry_finds_article() {
    // Setup: Backend A returns 430, Backend B returns 220
    let backend_a = spawn_mock_backend(|cmd| {
        if cmd.starts_with("ARTICLE") {
            b"430 No such article\r\n"
        } else {
            b"200 OK\r\n"
        }
    }).await;
    
    let backend_b = spawn_mock_backend(|cmd| {
        if cmd.starts_with("ARTICLE") {
            b"220 0 <test@example.com>\r\nBody\r\n.\r\n"
        } else {
            b"200 OK\r\n"
        }
    }).await;
    
    // Create proxy with availability cache
    let availability_cache = Arc::new(BackendAvailabilityCache::new(1000, Duration::from_secs(60)));
    let proxy = NntpProxy::builder(test_config())
        .with_availability_cache(availability_cache.clone())
        .build()?;
    
    // First request - should try both backends
    let mut client = connect_to_proxy().await?;
    send_command(&mut client, "ARTICLE <test@example.com>\r\n").await?;
    let response = read_response(&mut client).await?;
    
    // Should get article from backend B
    assert!(response.starts_with("220"));
    assert!(response.contains("Body"));
    
    // Check cache was updated
    let msgid = MessageId::from_borrowed("<test@example.com>")?;
    let availability = availability_cache.get(&msgid).await.unwrap();
    assert_eq!(availability.has(BackendId::from_index(0)), Some(false));  // Backend A doesn't have
    assert_eq!(availability.has(BackendId::from_index(1)), Some(true));   // Backend B has it
    
    // Second request - should go directly to backend B
    send_command(&mut client, "ARTICLE <test@example.com>\r\n").await?;
    let response2 = read_response(&mut client).await?;
    assert!(response2.starts_with("220"));
}
```

---

## Migration Path

**WARNING**: This is a breaking change in behavior, even for users with cache disabled.

### Phase 1: Restructure Cache (Major Refactor)
1. Implement `BackendAvailability` bitset in `src/cache/article.rs`
2. Implement `OwnedArticle` struct (replaces `CachedArticle`)
3. Delete `CachedArticle` type
4. Update `ArticleCache` to use `OwnedArticle` instead of `CachedArticle`
5. Add `ArticleCache::upsert()` and `mark_backend_missing()` methods
6. Support availability-only mode (skip storing headers/body when capacity=0)
7. Update all existing tests in `src/cache/article.rs`

### Phase 2: Eliminate Dual Code Paths (Integration)

**Files to modify**: `src/proxy.rs`, `src/session/handlers/per_command.rs`, `src/session/handlers/hybrid.rs`

1. **`src/proxy.rs`**:
   - Remove `Option<>` wrapper from cache field (both struct and builder)
   - Update `build()` to always create cache (use config.cache.max_capacity)
   - Update all session constructors to pass cache directly (not Option)

2. **`src/session/handlers/per_command.rs`**:
   - Remove `Option<>` wrapper from cache field
   - **DELETE** these functions entirely (~300 lines total):
     - `execute_command_with_caching()` - Replaced by unified logic
     - `execute_command_on_backend()` - Replaced by unified logic
     - `should_cache_command()` - All commands now use cache logic
     - All `test_should_cache_command_*()` tests
   - Modify `check_cache_and_serve()` to return `Option<OwnedArticle>` (no serving)
   - **Rewrite `route_and_execute_command()`** as single unified code path:
     - Remove `if should_cache` branching (lines 287, 315-327)
     - Always check cache first via `check_cache_and_serve()`
     - Serve from cache if `is_complete()`
     - Use `article.potential_backends()` for smart routing
     - Always parse backend responses with `protocol::Article::parse()`
     - Always spawn async cache updates via `upsert()` or `mark_backend_missing()`
     - Cache capacity determines what gets stored (availability vs full article)
     - Handle 430 responses with retry logic WITHOUT sending to client
     - Only send final response to client (after all retries)

3. **`src/session/handlers/hybrid.rs`**:
   - Remove `Option<>` wrapper from cache field
   - Replace `execute_command_on_backend()` calls (lines 67, 157) with unified logic:
     - Parse article commands and update cache
     - Stream non-article commands directly
   - For article commands (ARTICLE/BODY/HEAD/STAT by message-ID):
     - Check cache first
     - Update cache after backend response
     - Use smart routing for retries

4. **Update all session handler tests** to expect unified behavior
   - Remove tests for deleted functions
   - Add tests for availability-only mode (capacity=0)
   - Add tests for full cache mode (capacity>0)
   - Verify same code path handles both modes

### Phase 3: Test End-to-End (Validation)
1. Test with cache disabled (`--cache-capacity 0`): verify smart routing works
2. Test with cache enabled: verify full article caching works
3. Add new integration tests for backend availability tracking
4. Add tests for progressive enhancement (STAT → HEAD → ARTICLE)
5. Add tests for 430 retry logic without client response
6. Verify single code path handles both modes
7. Run in production with logging to validate cache behavior

### Phase 4: Optimize and Tune (Production)
1. Monitor cache hit rates by response type (STAT/HEAD/BODY/ARTICLE)
2. Monitor availability-only mode memory usage vs full cache mode
3. Tune cache capacity based on memory usage
4. Tune TTL based on 430 retry success rate
5. Add metrics for backend availability cache effectiveness

---

## Code References Summary

### Key Files to Modify

1. **`src/cache/article.rs`** (MAJOR REWRITE)
   - Delete `CachedArticle` struct
   - Add `BackendAvailability` bitset
   - Add `OwnedArticle` struct storing parsed article data + bitset
   - Modify `ArticleCache` to store `OwnedArticle` instead of `CachedArticle`
   - Add `upsert()` and `mark_backend_missing()` methods

2. **`src/proxy.rs`** (MODERATE CHANGES)
   - **NntpProxy struct** (line ~287): Change `cache: Option<Arc<ArticleCache>>` → `cache: Arc<ArticleCache>`
   - **NntpProxyBuilder struct** (line ~663): Change `cache: Option<Arc<ArticleCache>>` → `cache: Arc<ArticleCache>`
   - **Builder methods**: Remove all `Option` wrapping/unwrapping
     - `with_cache()` → always sets cache (not optional)
     - `build()` → always creates cache if not set (use capacity from config)
   - **Session creation**: Pass `Arc::clone(&self.cache)` instead of `self.cache.clone()`
   - Cache always created during proxy construction, configured via `--cache-capacity`:
     - 0 = availability-only mode (bitset only, no headers/body)
     - >0 = full cache mode (bitset + headers + body)

3. **`src/session/mod.rs`** (IF EXISTS - verify)
   - Update ClientSession enum variants to store `Arc<ArticleCache>` not `Option<>`
   - All session constructors take cache as required parameter

4. **`src/session/handlers/per_command.rs`** (MAJOR REWRITE)
   - Change `cache: Option<Arc<ArticleCache>>` → `cache: Arc<ArticleCache>`
   - **DELETE** `execute_command_with_caching()` entirely (~140 lines)
   - **DELETE** `execute_command_on_backend()` entirely (~140 lines)
   - **DELETE** `should_cache_command()` entirely (~15 lines)
   - **DELETE** all tests for `should_cache_command()` (~30 lines)
   - Update `check_cache_and_serve()` to return `Option<OwnedArticle>` without serving
   - **Rewrite** `route_and_execute_command()` as single unified code path
   - Always parse responses, always update cache, capacity determines storage

5. **`src/session/handlers/hybrid.rs`** (MODERATE REWRITE)
   - Change `cache: Option<Arc<ArticleCache>>` → `cache: Arc<ArticleCache>`
   - **Replace** calls to `execute_command_on_backend()` with unified cache logic
   - Lines 67, 157: Both calls must use cache-aware execution
   - Always parse responses and update cache for article commands
   - Non-article commands (GROUP, etc.) still stream through

6. **`src/session/handlers/standard.rs`** (IF EXISTS - verify)
   - Same changes as hybrid.rs: cache field and execution logic

3. **`Cargo.toml`** (NO CHANGES)
   - No new dependencies needed (already have moka for caching)

4. **Files to READ (understand, no changes)**:
   - `src/protocol/article/mod.rs` - Article<'a> parsing (zero-copy, validates responses)
   - `src/protocol/article/headers.rs` - Headers<'a> parsing
   - `src/router/mod.rs` - BackendSelector (for backend_count())
   - `src/session/backend.rs` - send_command_and_read_first_chunk()
   - `route_and_execute_command()` - use cached Article for smart routing
   - Add `execute_and_parse()` - new helper to execute command and parse response into Article
   - Add `parse_headers_to_smallvec()` - convert protocol::Headers to cache::Header SmallVec

3. **`Cargo.toml`** (ADD DEPENDENCY)
   - Add `smallvec = "1.13"` dependency for SmallVec

4. **No changes needed**:
   - `src/proxy.rs` - already has article_cache field
   - `src/config/types.rs` - already has CacheConfig
   - `src/protocol/article/mod.rs` - Article parsing stays the same (different type)

### Key Data Flow

```
Client ARTICLE <msgid>
    ↓
check_cache_and_serve() [per_command.rs:702]
    ↓
cache.get(&message_id) → Some(Article)
    ↓
Check Article.is_complete() → YES (has headers + body)
    ↓
Serve from cache, return Ok(())
    ────────────────────────────────────────
    
Alternative: Cache miss or incomplete
    ↓
route_and_execute_command() [per_command.rs:270]
    ↓
Get Article.potential_backends(2) → [Backend 0, Backend 1]
    ↓
Try Backend 0:
    execute_and_parse() [NEW]
        ↓
    send_command_and_read_first_chunk() [backend.rs:89]
        ↓
    Read response: "430 No such article\r\n"
        ↓
    Parse response: StatusCode(430), SingleLine [response.rs:159]
        ↓
    Return (Err, Some(StatusCode(430)), None)
        ↓
    Spawn async: cache.update(msgid, |article| {
        article.mark_backend_missing(Backend 0);
    })
        ↓
    Check: 430 && attempt < backends_to_try.len() - 1 → YES
        ↓
Try Backend 1:
    execute_and_parse()
        ↓
    send_command_and_read_first_chunk()
        ↓
    Read response: "220 0 <msgid>\r\nHeaders\r\n\r\nBody\r\n.\r\n"
        ↓
    Parse response: StatusCode(220), MultilineData [response.rs:159]
        ↓
    Parse into Article::from_article(Backend 1, headers, body)
        ↓
    Write response to client
        ↓
    Return (Ok(()), Some(StatusCode(220)), Some(Article))
        ↓
    Spawn async: cache.insert(msgid, Article {
        backend_availability: 0b10,  // Backend 1 has it, Backend 0 doesn't
        headers: parsed_headers,
        body: Some(body_data),
        ...
    })
        ↓
Return success to client
```

---

## Open Questions

1. **TTL Strategy**: 
   - Short TTL (minutes): Adapts to backend changes quickly
   - Long TTL (hours): Better cache hit rate, less adaptive
   - **Recommendation**: Start with 1 hour, tune based on 430 rate

2. **Cache Capacity**:
   - 100K entries: ~3.4MB RAM
   - Typical NZB: 10K-100K articles
   - **Recommendation**: 100K default, configurable

3. **Retry Order**:
   - Try all "not checked" backends first?
   - Try backends in round-robin order?
   - **Recommendation**: Round-robin among potential backends (fairness)

4. **STAT Integration**:
   - Should proxy automatically STAT before ARTICLE?
   - Let client control (client sends STAT if it wants)?
   - **Recommendation**: Let client control (don't add latency by default)

5. **Monitoring**:
   - Track cache hit rate
   - Track 430-then-retry-success rate
   - Track latency impact of retries
   - **Recommendation**: Add metrics for all three

---

## Success Metrics

1. **430 Reduction**: Track 430 responses before/after
   - Target: 50%+ reduction in 430 errors from ARTICLE/BODY commands
   - Measure: Count 430 responses per 1000 ARTICLE requests
   - **Exclude STAT/HEAD 430s** from this metric (expected due to false positives)

2. **Cache Hit Rate**: Percentage of requests served from cache (full cache mode only)
   - Target: 60%+ hit rate for typical NZB workloads
   - Measure: Cache hits / (cache hits + cache misses)
   - **Note**: Only applicable when `--cache-capacity > 0`

3. **Smart Routing Success**: Requests routed to correct backend first try
   - Target: 80%+ first-backend success rate for ARTICLE/BODY
   - Measure: Successful responses without retry / total requests
   - **Note**: May be lower initially due to conservative STAT/HEAD 430 handling
   - **Applicable to both cache modes** (availability-only and full cache)

4. **Memory Usage**: Cache memory footprint
   - **Availability-only mode** (`--cache-capacity 0`): <1MB for 10K articles
   - **Full cache mode** (`--cache-capacity > 0`): <110MB for 10K articles (mixed workload)
   - Measure: RSS increase after cache warm-up

5. **False Negative Rate**: Backend marked as missing but actually has article
   - Target: 0% (should never happen - all 430s are trusted)
   - Measure: Track instances where cached 430 contradicted by later success

---

## Conclusion

This design adds intelligent backend selection based on learned availability patterns, addressing the core issue of unnecessary 430 responses when another backend has the article. The unified cache approach stores structured article data with backend availability bitsets, enabling:

1. **Smart routing** - Always route to backends likely to have the article
2. **430 retry** - Automatically try other backends on 430 responses
3. **Progressive enhancement** - Cache entries improve from STAT → HEAD → ARTICLE
4. **Two modes**:
   - **Availability-only** (`--cache-capacity 0`): ~68 bytes/article, smart routing only
   - **Full cache** (`--cache-capacity > 0`): Smart routing + article caching

**Breaking Change**: This fundamentally changes proxy behavior. Even with cache disabled, the proxy now:

- Tracks which backends have which articles
- Retries 430 responses on other backends
- Routes future requests to backends likely to succeed

**Code Impact**: Removes ~300 lines of duplicate logic from `per_command.rs`, eliminates `Option<ArticleCache>` throughout codebase, consolidates three separate execution paths (cache/no-cache/hybrid) into one unified flow.

**Performance**: Memory-efficient (68 bytes to ~50KB per article depending on mode and response type), fast (<25ns overhead for availability checks), single unified code path for both modes.
