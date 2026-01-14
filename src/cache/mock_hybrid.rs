//! Mock hybrid cache for testing
//!
//! Provides a simple in-memory implementation that mimics HybridArticleCache
//! behavior without foyer's complexity. Used in tests to avoid foyer's
//! runtime issues.

use super::{ArticleAvailability, HybridArticleEntry, HybridCacheStats};
use crate::types::{BackendId, MessageId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Mock hybrid cache for testing
#[derive(Clone)]
pub struct MockHybridCache {
    storage: Arc<Mutex<HashMap<String, HybridArticleEntry>>>,
    hits: Arc<Mutex<u64>>,
    misses: Arc<Mutex<u64>>,
    disk_hits: Arc<Mutex<u64>>,
}

impl MockHybridCache {
    pub fn new(_memory_capacity: u64) -> Self {
        Self {
            storage: Arc::new(Mutex::new(HashMap::new())),
            hits: Arc::new(Mutex::new(0)),
            misses: Arc::new(Mutex::new(0)),
            disk_hits: Arc::new(Mutex::new(0)),
        }
    }

    pub async fn get<'a>(&self, message_id: &MessageId<'a>) -> Option<HybridArticleEntry> {
        let key = message_id.without_brackets().to_string();
        let storage = self.storage.lock().unwrap();

        if let Some(entry) = storage.get(&key) {
            *self.hits.lock().unwrap() += 1;
            Some(entry.clone())
        } else {
            *self.misses.lock().unwrap() += 1;
            None
        }
    }

    pub async fn upsert<'a>(
        &self,
        message_id: MessageId<'a>,
        buffer: Vec<u8>,
        backend_id: BackendId,
    ) {
        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        // Check for existing entry - don't overwrite larger with smaller
        if let Some(existing) = storage.get(&key)
            && existing.buffer().len() > buffer.len()
        {
            // Just update availability
            let mut updated = existing.clone();
            updated.record_backend_has(backend_id);
            storage.insert(key, updated);
            return;
        }

        if let Some(mut entry) = HybridArticleEntry::new(buffer) {
            entry.record_backend_has(backend_id);
            storage.insert(key, entry);
        }
    }

    pub async fn record_missing<'a>(&self, message_id: MessageId<'a>, backend_id: BackendId) {
        let key = message_id.without_brackets().to_string();
        let mut storage = self.storage.lock().unwrap();

        let entry = match storage.get(&key) {
            Some(existing) => {
                let mut updated = existing.clone();
                updated.record_backend_missing(backend_id);
                updated
            }
            None => {
                let mut entry = HybridArticleEntry::new(b"430\r\n".to_vec()).expect("430 is valid");
                entry.record_backend_missing(backend_id);
                entry
            }
        };

        storage.insert(key, entry);
    }

    pub async fn sync_availability<'a>(
        &self,
        message_id: MessageId<'a>,
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
                        HybridArticleEntry::new(b"430\r\n".to_vec()).expect("430 is valid");
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

    pub fn stats(&self) -> HybridCacheStats {
        HybridCacheStats {
            hits: *self.hits.lock().unwrap(),
            misses: *self.misses.lock().unwrap(),
            disk_hits: *self.disk_hits.lock().unwrap(),
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
