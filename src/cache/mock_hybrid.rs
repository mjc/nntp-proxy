//! Mock hybrid cache for testing
//!
//! Provides a simple in-memory implementation that mimics `HybridArticleCache`
//! behavior without foyer's complexity. Used in tests to avoid foyer's
//! runtime issues.
// Methods are async to match the HybridArticleCache trait interface; no internal awaits needed.
#![allow(clippy::unused_async)]

use super::hybrid_codec::DiskCachedArticle;
use super::{BackendResponseBytes, HybridCacheStats};
use crate::types::{BackendId, MessageId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Mock hybrid cache for testing
#[derive(Clone)]
struct MockHybridCache {
    storage: Arc<Mutex<HashMap<String, DiskCachedArticle>>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
    disk_hits: Arc<AtomicU64>,
}

impl MockHybridCache {
    #[must_use]
    fn new(_memory_capacity: u64) -> Self {
        Self {
            storage: Arc::new(Mutex::new(HashMap::new())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            disk_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn get(&self, message_id: &MessageId<'_>) -> Option<DiskCachedArticle> {
        let key = message_id.without_brackets().to_string();
        let storage = self.storage.lock().unwrap();

        if let Some(entry) = storage.get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(entry.clone())
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    async fn upsert_backend_response(
        &self,
        message_id: MessageId<'_>,
        buffer: impl AsRef<[u8]>,
        backend_id: BackendId,
    ) {
        self.upsert_ingest(message_id, buffer.as_ref().into(), backend_id)
            .await;
    }

    async fn upsert_ingest(
        &self,
        message_id: MessageId<'_>,
        buffer: BackendResponseBytes,
        backend_id: BackendId,
    ) {
        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        let Some(mut entry) =
            DiskCachedArticle::from_ingest_bytes_with_tier(buffer, super::ttl::CacheTier::new(0))
        else {
            return;
        };
        let entry_len = entry.payload_len();

        // Check for existing entry - don't overwrite larger semantic payloads with smaller ones.
        if let Some(existing) = storage.get(&key)
            && existing.payload_len() > entry_len
        {
            // Just update availability
            let mut updated = existing.clone();
            updated.record_backend_has(backend_id);
            storage.insert(key, updated);
            return;
        }

        entry.record_backend_has(backend_id);
        storage.insert(key, entry);
    }

    async fn record_missing(&self, message_id: MessageId<'_>, backend_id: BackendId) {
        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        let entry = if let Some(existing) = storage.get(&key) {
            let mut updated = existing.clone();
            updated.record_backend_missing(backend_id);
            updated
        } else {
            let mut entry = DiskCachedArticle::missing(super::ttl::CacheTier::new(0));
            entry.record_backend_missing(backend_id);
            entry
        };

        storage.insert(key, entry);
    }

    #[must_use]
    fn stats(&self) -> HybridCacheStats {
        HybridCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            memory_capacity: 0,
            disk_capacity: 0,
            disk_write_bytes: 0,
            disk_read_bytes: 0,
            disk_write_ios: 0,
            disk_read_ios: 0,
        }
    }

    async fn close(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestKind;
    use anyhow::Result;
    use futures::executor::block_on;

    fn msg_id() -> MessageId<'static> {
        MessageId::from_borrowed("<mock-hybrid@example>").expect("valid message id")
    }

    fn msgid(value: &str) -> MessageId<'_> {
        MessageId::from_borrowed(value).unwrap()
    }

    fn render_response(
        entry: &DiskCachedArticle,
        request_kind: RequestKind,
        message_id: &MessageId<'_>,
    ) -> Option<Vec<u8>> {
        let response = entry.cached_response_for(request_kind, message_id.as_str())?;
        let mut out = Vec::with_capacity(response.wire_len().get());
        block_on(response.write_to(&mut out)).ok()?;
        Some(out)
    }

    fn assert_article(entry: &DiskCachedArticle, message_id: &MessageId<'_>, expected: &[u8]) {
        assert_eq!(
            render_response(entry, RequestKind::Article, message_id).unwrap(),
            expected
        );
    }

    fn assert_availability(entry: &DiskCachedArticle, cases: &[(usize, bool)]) {
        cases.iter().for_each(|(backend_index, should_try)| {
            assert_eq!(
                entry.should_try_backend(BackendId::from_index(*backend_index)),
                *should_try,
                "backend {backend_index}"
            );
        });
    }

    #[tokio::test]
    async fn upsert_keeps_existing_semantic_payload_over_longer_metadata_only_response() {
        let cache = MockHybridCache::new(1024);
        cache
            .upsert_backend_response(
                msg_id(),
                b"220 1 <mock-hybrid@example>\r\nH: V\r\n\r\nBody\r\n.\r\n".as_slice(),
                BackendId::from_index(0),
            )
            .await;

        cache
            .upsert_backend_response(
                msg_id(),
                b"220 1 <mock-hybrid@example> long status line without payload\r\n".as_slice(),
                BackendId::from_index(1),
            )
            .await;

        let entry = cache.get(&msg_id()).await.expect("entry remains cached");
        assert!(
            entry
                .cached_response_for(RequestKind::Article, "<mock-hybrid@example>")
                .is_some(),
            "longer metadata-only responses must not replace semantic article payloads"
        );
        assert!(entry.should_try_backend(BackendId::from_index(1)));
    }

    #[tokio::test]
    async fn basic_ops() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);

        let message_id = msgid("<test@example.com>");
        let buffer = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";

        cache
            .upsert_backend_response(
                message_id.clone(),
                buffer.as_slice(),
                BackendId::from_index(0),
            )
            .await;

        assert_article(
            &cache.get(&message_id).await.expect("Entry should exist"),
            &message_id,
            buffer,
        );

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 0);

        Ok(())
    }

    #[tokio::test]
    async fn upsert_accepts_borrowed_backend_bytes() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);
        let message_id = msgid("<borrowed@example.com>");
        let buffer = b"220 0 <borrowed@example.com>\r\nSubject: Test\r\n\r\nBody\r\n.\r\n";

        cache
            .upsert_backend_response(
                message_id.clone(),
                buffer.as_slice(),
                BackendId::from_index(0),
            )
            .await;

        assert_article(
            &cache.get(&message_id).await.expect("cached entry"),
            &message_id,
            buffer,
        );

        Ok(())
    }

    #[tokio::test]
    async fn cache_miss_updates_stats() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);

        let message_id = msgid("<nonexistent@example.com>");
        let entry = cache.get(&message_id).await;

        assert!(entry.is_none());

        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 1);

        Ok(())
    }

    #[tokio::test]
    async fn upsert_preserves_larger_buffer() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);

        let message_id = msgid("<test@example.com>");

        let large_buffer =
            b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nLarge body content here\r\n.\r\n";
        cache
            .upsert_backend_response(
                message_id.clone(),
                large_buffer.as_slice(),
                BackendId::from_index(0),
            )
            .await;

        cache
            .upsert_backend_response(
                message_id.clone(),
                b"223 0 <test@example.com>\r\n".as_slice(),
                BackendId::from_index(1),
            )
            .await;

        let entry = cache.get(&message_id).await.unwrap();
        assert_article(&entry, &message_id, large_buffer);
        assert_availability(&entry, &[(0, true), (1, true)]);

        Ok(())
    }

    #[tokio::test]
    async fn record_missing_creates_availability_entry() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);

        let message_id = msgid("<missing@example.com>");

        cache
            .record_missing(message_id.clone(), BackendId::from_index(0))
            .await;

        let entry = cache.get(&message_id).await;
        assert!(entry.is_some());

        let entry = entry.unwrap();
        assert_availability(&entry, &[(0, false), (1, true)]);

        Ok(())
    }

    #[tokio::test]
    async fn tracks_availability() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);

        let message_id = msgid("<avail@example.com>");

        cache
            .upsert_backend_response(
                message_id.clone(),
                b"220 0 <avail@example.com>\r\nBody\r\n.\r\n".as_slice(),
                BackendId::from_index(0),
            )
            .await;

        cache
            .record_missing(message_id.clone(), BackendId::from_index(1))
            .await;
        cache
            .record_missing(message_id.clone(), BackendId::from_index(2))
            .await;

        let entry = cache.get(&message_id).await.unwrap();
        assert_availability(&entry, &[(0, true), (1, false), (2, false), (3, true)]);

        Ok(())
    }

    #[tokio::test]
    async fn close_succeeds() -> Result<()> {
        let cache = MockHybridCache::new(1024 * 1024);

        let message_id = msgid("<test@example.com>");
        cache
            .upsert_backend_response(
                message_id,
                b"220 0 <test@example.com>\r\n.\r\n".as_slice(),
                BackendId::from_index(0),
            )
            .await;

        cache.close().await?;

        Ok(())
    }
}
