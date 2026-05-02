//! Mock hybrid cache for testing
//!
//! Provides a simple in-memory implementation that mimics `HybridArticleCache`
//! behavior without foyer's complexity. Used in tests to avoid foyer's
//! runtime issues.
// Methods are async to match the HybridArticleCache trait interface; no internal awaits needed.
#![allow(clippy::unused_async)]

use super::{ArticleAvailability, CacheIngestBytes, HybridArticleEntry, HybridCacheStats};
use crate::types::{BackendId, MessageId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Mock hybrid cache for testing
#[derive(Clone)]
pub struct MockHybridCache {
    storage: Arc<Mutex<HashMap<String, HybridArticleEntry>>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
    disk_hits: Arc<AtomicU64>,
}

impl MockHybridCache {
    #[must_use]
    pub fn new(_memory_capacity: u64) -> Self {
        Self {
            storage: Arc::new(Mutex::new(HashMap::new())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            disk_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn get(&self, message_id: &MessageId<'_>) -> Option<HybridArticleEntry> {
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

    pub async fn upsert(
        &self,
        message_id: MessageId<'_>,
        buffer: impl AsRef<[u8]>,
        backend_id: BackendId,
    ) {
        self.upsert_ingest(message_id, buffer.as_ref().into(), backend_id)
            .await;
    }

    pub(crate) async fn upsert_ingest(
        &self,
        message_id: MessageId<'_>,
        buffer: CacheIngestBytes,
        backend_id: BackendId,
    ) {
        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        let Some(mut entry) =
            HybridArticleEntry::from_ingest_bytes_with_tier(buffer, super::ttl::CacheTier::new(0))
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

    pub async fn record_missing(&self, message_id: MessageId<'_>, backend_id: BackendId) {
        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        let entry = if let Some(existing) = storage.get(&key) {
            let mut updated = existing.clone();
            updated.record_backend_missing(backend_id);
            updated
        } else {
            let mut entry =
                HybridArticleEntry::from_response_bytes(b"430\r\n").expect("430 is valid");
            entry.record_backend_missing(backend_id);
            entry
        };

        storage.insert(key, entry);
    }

    pub async fn sync_availability(
        &self,
        message_id: MessageId<'_>,
        availability: &ArticleAvailability,
    ) {
        if availability.checked_bits() == 0 {
            return;
        }

        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        let updated = match storage.get(&key) {
            Some(existing) => {
                let mut entry = existing.clone();
                // Manually merge availability bits
                for i in 0..8 {
                    let backend_id = BackendId::from_index(i);
                    if availability.should_try(backend_id) {
                        entry.record_backend_has(backend_id);
                    } else if availability.is_missing(backend_id) {
                        entry.record_backend_missing(backend_id);
                    }
                }
                Some(entry)
            }
            None => {
                if availability.any_backend_has_article() {
                    None
                } else {
                    let mut entry =
                        HybridArticleEntry::from_response_bytes(b"430\r\n").expect("430 is valid");
                    for i in 0..8 {
                        let backend_id = BackendId::from_index(i);
                        if availability.should_try(backend_id) {
                            entry.record_backend_has(backend_id);
                        } else if availability.is_missing(backend_id) {
                            entry.record_backend_missing(backend_id);
                        }
                    }
                    Some(entry)
                }
            }
        };

        if let Some(entry) = updated {
            storage.insert(key, entry);
        }
    }

    #[must_use]
    pub fn stats(&self) -> HybridCacheStats {
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

    pub async fn close(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestKind;

    fn msg_id() -> MessageId<'static> {
        MessageId::from_borrowed("<mock-hybrid@example>").expect("valid message id")
    }

    #[tokio::test]
    async fn upsert_keeps_existing_semantic_payload_over_longer_wire_stub() {
        let cache = MockHybridCache::new(1024);
        cache
            .upsert(
                msg_id(),
                b"220 1 <mock-hybrid@example>\r\nH: V\r\n\r\nBody\r\n.\r\n".as_slice(),
                BackendId::from_index(0),
            )
            .await;

        cache
            .upsert(
                msg_id(),
                b"220 1 <mock-hybrid@example> long status line without payload\r\n".as_slice(),
                BackendId::from_index(1),
            )
            .await;

        let entry = cache.get(&msg_id()).await.expect("entry remains cached");
        assert!(
            entry
                .response_parts_for_request_kind(RequestKind::Article, "<mock-hybrid@example>")
                .is_some(),
            "longer raw wire stubs must not replace semantic article payloads"
        );
        assert!(entry.should_try_backend(BackendId::from_index(1)));
    }
}
