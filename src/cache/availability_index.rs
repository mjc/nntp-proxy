//! Compact availability-only negative index.
//!
//! Stores exact per-message-id 430 knowledge without any payload bytes. Lookups are
//! zero-allocation on the request path; inserts copy message-id bytes into a
//! preallocated shard-local arena. When a shard fills, it evicts the least
//! recently used entries and compacts the shard so bounded memory stays usable.

use super::{ArticleAvailability, CachedArticle, ttl};
use crate::io_util::atomic_replace_file;
use crate::types::{BackendId, MessageId};
use anyhow::{Context, Result};
use std::fs;
use std::mem::{size_of, take};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use twox_hash::XxHash64;

const DEFAULT_SHARDS: usize = 16;
const SLOT_BUDGET_DIVISOR: usize = 3;
const LOAD_FACTOR_NUM: usize = 85;
const LOAD_FACTOR_DEN: usize = 100;
const PERSISTENCE_MAGIC: &[u8; 8] = b"ANEGIDX2";
const LEGACY_PERSISTENCE_MAGIC: &[u8; 8] = b"ANEGIDX1";

static SAVE_LOCK: Mutex<()> = Mutex::new(());
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    hash: u64,
    offset: u32,
    len: u16,
    missing: u8,
    occupied: bool,
    last_touch: u64,
    inserted_at: u64,
}

impl Slot {
    const fn is_empty(self) -> bool {
        !self.occupied
    }
}

#[derive(Clone, Debug)]
struct PersistedEntry {
    key: Vec<u8>,
    missing: u8,
    last_touch: u64,
    inserted_at: u64,
}

#[derive(Debug)]
struct Shard {
    slots: Box<[Slot]>,
    len: usize,
    arena: Box<[u8]>,
    arena_len: usize,
}

impl Shard {
    fn new(slot_count: usize, arena_bytes: usize) -> Self {
        Self {
            slots: vec![Slot::default(); slot_count].into_boxed_slice(),
            len: 0,
            arena: vec![0; arena_bytes].into_boxed_slice(),
            arena_len: 0,
        }
    }

    fn mask(&self) -> usize {
        self.slots.len().saturating_sub(1)
    }

    fn can_insert(&self) -> bool {
        !self.slots.is_empty()
            && self.len.saturating_mul(LOAD_FACTOR_DEN) < self.slots.len() * LOAD_FACTOR_NUM
    }

    fn key_eq(&self, slot: Slot, key: &[u8]) -> bool {
        self.entry_bytes(slot) == Some(key)
    }

    fn entry_bytes(&self, slot: Slot) -> Option<&[u8]> {
        let start = slot.offset as usize;
        let end = start.checked_add(slot.len as usize)?;
        self.arena.get(start..end)
    }

    fn probe_distance(&self, hash: u64, idx: usize) -> usize {
        let home = (hash as usize) & self.mask();
        idx.wrapping_sub(home) & self.mask()
    }

    fn lookup_missing(
        &mut self,
        hash: u64,
        key: &[u8],
        touch: u64,
        base_ttl: ttl::CacheTtlMillis,
    ) -> Option<u8> {
        let index = self.lookup_slot_index(hash, key)?;
        let slot = self.slots[index];
        if ttl::is_expired(
            ttl::CacheTimestampMillis::new(slot.inserted_at),
            base_ttl,
            ttl::CacheTier::new(0),
        ) {
            self.rebuild_excluding(index);
            return None;
        }
        let slot = &mut self.slots[index];
        slot.last_touch = touch;
        Some(slot.missing)
    }

    fn insert_missing(
        &mut self,
        hash: u64,
        key: &[u8],
        missing_bits: u8,
        touch: u64,
        inserted_at: u64,
    ) -> usize {
        if missing_bits == 0 || key.is_empty() {
            return 0;
        }

        if let Some(existing) = self.lookup_slot_index(hash, key) {
            let slot = &mut self.slots[existing];
            slot.missing |= missing_bits;
            slot.last_touch = touch;
            slot.inserted_at = inserted_at;
            return 0;
        }

        let Some(evicted) = self.ensure_space_for(key.len()) else {
            return usize::MAX;
        };

        let new_slot = Slot {
            hash,
            offset: self.allocate_key_bytes(key),
            len: key.len() as u16,
            missing: missing_bits,
            occupied: true,
            last_touch: touch,
            inserted_at,
        };
        debug_assert!(self.place_slot(new_slot));
        evicted
    }

    fn restore_entry(
        &mut self,
        hash: u64,
        key: &[u8],
        missing_bits: u8,
        last_touch: u64,
        inserted_at: u64,
    ) -> bool {
        if missing_bits == 0 || key.is_empty() {
            return true;
        }

        if let Some(existing) = self.lookup_slot_index(hash, key) {
            let slot = &mut self.slots[existing];
            slot.missing |= missing_bits;
            slot.last_touch = slot.last_touch.max(last_touch);
            slot.inserted_at = slot.inserted_at.max(inserted_at);
            return true;
        }

        if self.ensure_space_for(key.len()).is_none() {
            return false;
        }

        let new_slot = Slot {
            hash,
            offset: self.allocate_key_bytes(key),
            len: key.len() as u16,
            missing: missing_bits,
            occupied: true,
            last_touch,
            inserted_at,
        };
        self.place_slot(new_slot)
    }

    fn allocate_key_bytes(&mut self, key: &[u8]) -> u32 {
        let offset = self.arena_len;
        let end = offset + key.len();
        self.arena[offset..end].copy_from_slice(key);
        self.arena_len = end;
        offset as u32
    }

    fn place_slot(&mut self, mut new_slot: Slot) -> bool {
        if self.slots.is_empty() {
            return false;
        }

        let mut idx = (new_slot.hash as usize) & self.mask();
        let mut distance = 0usize;
        loop {
            if self.slots[idx].is_empty() {
                self.slots[idx] = new_slot;
                self.len += 1;
                return true;
            }

            let resident_distance = self.probe_distance(self.slots[idx].hash, idx);
            if resident_distance < distance {
                std::mem::swap(&mut self.slots[idx], &mut new_slot);
                distance = resident_distance;
            }

            idx = (idx + 1) & self.mask();
            distance += 1;
            if distance > self.mask() {
                return false;
            }
        }
    }

    fn ensure_space_for(&mut self, required_key_len: usize) -> Option<usize> {
        if self.slots.is_empty()
            || required_key_len == 0
            || required_key_len > self.arena.len()
            || required_key_len > u16::MAX as usize
        {
            return None;
        }

        let mut evicted = 0usize;
        while (!self.can_insert() || self.arena_len + required_key_len > self.arena.len())
            && self.len > 0
        {
            self.evict_lru()?;
            evicted += 1;
        }

        if self.can_insert() && self.arena_len + required_key_len <= self.arena.len() {
            Some(evicted)
        } else {
            None
        }
    }

    fn evict_lru(&mut self) -> Option<()> {
        let victim = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.occupied)
            .min_by_key(|(_, slot)| slot.last_touch)
            .map(|(idx, _)| idx)?;
        self.rebuild_excluding(victim);
        Some(())
    }

    fn rebuild_excluding(&mut self, victim_idx: usize) {
        let slot_count = self.slots.len();
        let arena_bytes = self.arena.len();
        let old_slots = take(&mut self.slots);
        let old_arena = take(&mut self.arena);

        let mut rebuilt = Shard::new(slot_count, arena_bytes);
        for (idx, slot) in old_slots.iter().copied().enumerate() {
            if idx == victim_idx || slot.is_empty() {
                continue;
            }

            if let Some(key) =
                old_arena.get(slot.offset as usize..slot.offset as usize + slot.len as usize)
            {
                let inserted = rebuilt.restore_entry(
                    slot.hash,
                    key,
                    slot.missing,
                    slot.last_touch,
                    slot.inserted_at,
                );
                debug_assert!(
                    inserted,
                    "rebuilding shard should preserve existing entries"
                );
            }
        }

        *self = rebuilt;
    }

    fn lookup_slot_index(&self, hash: u64, key: &[u8]) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }

        let mut idx = (hash as usize) & self.mask();
        let mut distance = 0usize;
        loop {
            let slot = self.slots[idx];
            if slot.is_empty() {
                return None;
            }
            if self.probe_distance(slot.hash, idx) < distance {
                return None;
            }
            if slot.hash == hash && self.key_eq(slot, key) {
                return Some(idx);
            }
            idx = (idx + 1) & self.mask();
            distance += 1;
            if distance > self.mask() {
                return None;
            }
        }
    }

    fn clear(&mut self) {
        *self = Shard::new(self.slots.len(), self.arena.len());
    }
}

#[derive(Debug)]
pub struct AvailabilityIndex {
    shards: Box<[Mutex<Shard>]>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    dropped: AtomicU64,
    evictions: AtomicU64,
    clock: AtomicU64,
    ttl: ttl::CacheTtlMillis,
}

impl AvailabilityIndex {
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        Self::with_ttl(capacity_bytes, Duration::MAX)
    }

    #[must_use]
    pub fn with_ttl(capacity_bytes: u64, ttl: Duration) -> Self {
        Self::with_ttl_and_shard_count(capacity_bytes, ttl, DEFAULT_SHARDS)
    }

    #[must_use]
    #[cfg(test)]
    fn with_shard_count(capacity_bytes: u64, shard_limit: usize) -> Self {
        Self::with_ttl_and_shard_count(capacity_bytes, Duration::MAX, shard_limit)
    }

    #[must_use]
    fn with_ttl_and_shard_count(capacity_bytes: u64, ttl: Duration, shard_limit: usize) -> Self {
        let ttl = ttl::CacheTtlMillis::from_duration(ttl);
        let total_bytes = capacity_bytes as usize;
        if total_bytes == 0 {
            return Self {
                shards: Box::new([Mutex::new(Shard::new(0, 0))]),
                capacity_bytes,
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                inserts: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                evictions: AtomicU64::new(0),
                clock: AtomicU64::new(0),
                ttl,
            };
        }

        let shard_count = shard_limit.min(total_bytes.max(1));
        let slot_budget = total_bytes / SLOT_BUDGET_DIVISOR;
        let arena_budget = total_bytes.saturating_sub(slot_budget);
        let slot_bytes_per_shard = slot_budget / shard_count;
        let arena_bytes_per_shard = arena_budget / shard_count;
        let slot_count = next_power_of_two(
            (slot_bytes_per_shard / size_of::<Slot>())
                .max(2)
                .saturating_sub(1),
        );

        let shards = (0..shard_count)
            .map(|_| Mutex::new(Shard::new(slot_count, arena_bytes_per_shard)))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            capacity_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            clock: AtomicU64::new(0),
            ttl,
        }
    }

    #[must_use]
    pub fn get(&self, message_id: &MessageId<'_>) -> Option<CachedArticle> {
        self.lookup_by_key(message_id.without_brackets())
    }

    #[must_use]
    pub fn get_request_message_id(&self, message_id: &str) -> Option<CachedArticle> {
        let key = message_id.strip_prefix('<')?.strip_suffix('>')?;
        self.lookup_by_key(key)
    }

    pub fn record_backend_missing(&self, message_id: &MessageId<'_>, backend_id: BackendId) {
        self.insert_missing_bits(message_id.without_brackets(), backend_id.availability_bit());
    }

    pub fn sync_availability(
        &self,
        message_id: &MessageId<'_>,
        availability: &ArticleAvailability,
    ) {
        self.insert_missing_bits(message_id.without_brackets(), availability.missing_bits());
    }

    pub fn load_from_path(&self, path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }

        let data = fs::read(path).with_context(|| {
            format!("Failed to read availability index from {}", path.display())
        })?;
        let mut entries = parse_entries(&data)?;
        entries.sort_by_key(|entry| entry.last_touch);

        self.reset();
        let mut max_touch = 0u64;
        for entry in entries {
            if self.is_expired(entry.inserted_at) {
                continue;
            }
            max_touch = max_touch.max(entry.last_touch);
            self.restore_entry(
                &entry.key,
                entry.missing,
                entry.last_touch,
                entry.inserted_at,
            );
        }
        self.clock.store(max_touch, Ordering::Relaxed);
        Ok(true)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let _save_guard = SAVE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("availability save lock poisoned"))?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create availability directory {}",
                    parent.display()
                )
            })?;
        }

        let entries = self.snapshot_entries();
        let total_key_bytes: usize = entries.iter().map(|entry| entry.key.len()).sum();
        let mut bytes = Vec::with_capacity(16 + entries.len() * 19 + total_key_bytes);
        bytes.extend_from_slice(PERSISTENCE_MAGIC);
        bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&(entry.key.len() as u16).to_le_bytes());
            bytes.push(entry.missing);
            bytes.extend_from_slice(&entry.last_touch.to_le_bytes());
            bytes.extend_from_slice(&entry.inserted_at.to_le_bytes());
            bytes.extend_from_slice(&entry.key);
        }

        let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_filename = format!(
            "{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            seq
        );
        let tmp_path = path.with_file_name(tmp_filename);
        fs::write(&tmp_path, bytes).with_context(|| {
            format!(
                "Failed to write availability index to {}",
                tmp_path.display()
            )
        })?;

        if let Err(e) = atomic_replace_file(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }

        Ok(())
    }

    #[must_use]
    pub const fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.shards
            .iter()
            .map(|shard| shard.lock().unwrap_or_else(|e| e.into_inner()).len as u64)
            .sum()
    }

    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.shards
            .iter()
            .map(|shard| {
                let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
                (shard.slots.len() * size_of::<Slot>() + shard.arena_len) as u64
            })
            .sum()
    }

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

    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    fn lookup_by_key(&self, key: &str) -> Option<CachedArticle> {
        let hash = hash_key(key.as_bytes());
        let shard = self.shard_for(hash);
        let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
        let result = shard.lookup_missing(hash, key.as_bytes(), self.next_touch(), self.ttl);
        drop(shard);

        match result {
            Some(bits) if bits != 0 => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(CachedArticle::negative_only(bits))
            }
            _ => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    fn insert_missing_bits(&self, key: &str, missing_bits: u8) {
        if missing_bits == 0 || key.is_empty() {
            return;
        }

        let hash = hash_key(key.as_bytes());
        let shard = self.shard_for(hash);
        let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
        match shard.insert_missing(
            hash,
            key.as_bytes(),
            missing_bits,
            self.next_touch(),
            ttl::now_millis(),
        ) {
            usize::MAX => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            evicted => {
                self.inserts.fetch_add(1, Ordering::Relaxed);
                if evicted != 0 {
                    self.evictions.fetch_add(evicted as u64, Ordering::Relaxed);
                }
            }
        }
    }

    fn restore_entry(&self, key: &[u8], missing_bits: u8, last_touch: u64, inserted_at: u64) {
        if missing_bits == 0 || key.is_empty() {
            return;
        }

        let hash = hash_key(key);
        let shard = self.shard_for(hash);
        let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
        if !shard.restore_entry(hash, key, missing_bits, last_touch, inserted_at) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn reset(&self) {
        for shard in &self.shards {
            shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.inserts.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.evictions.store(0, Ordering::Relaxed);
        self.clock.store(0, Ordering::Relaxed);
    }

    fn snapshot_entries(&self) -> Vec<PersistedEntry> {
        let mut entries = Vec::with_capacity(self.entry_count() as usize);
        for shard in &self.shards {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            for slot in shard.slots.iter().copied().filter(|slot| slot.occupied) {
                if self.is_expired(slot.inserted_at) {
                    continue;
                }
                if let Some(key) = shard.entry_bytes(slot) {
                    entries.push(PersistedEntry {
                        key: key.to_vec(),
                        missing: slot.missing,
                        last_touch: slot.last_touch,
                        inserted_at: slot.inserted_at,
                    });
                }
            }
        }
        entries
    }

    fn next_touch(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn shard_for(&self, hash: u64) -> &Mutex<Shard> {
        &self.shards[(hash as usize) % self.shards.len()]
    }

    fn is_expired(&self, inserted_at: u64) -> bool {
        ttl::is_expired(
            ttl::CacheTimestampMillis::new(inserted_at),
            self.ttl,
            ttl::CacheTier::new(0),
        )
    }
}

fn parse_entries(data: &[u8]) -> Result<Vec<PersistedEntry>> {
    if data.len() < PERSISTENCE_MAGIC.len() + size_of::<u64>() {
        anyhow::bail!("availability index file too short");
    }
    let magic = &data[..PERSISTENCE_MAGIC.len()];
    if magic == LEGACY_PERSISTENCE_MAGIC {
        return Ok(Vec::new());
    }
    if magic != PERSISTENCE_MAGIC {
        anyhow::bail!("unknown availability index format");
    }

    let mut cursor = PERSISTENCE_MAGIC.len();
    let entry_count = read_u64(data, &mut cursor)? as usize;
    let mut entries = Vec::with_capacity(entry_count);

    for _ in 0..entry_count {
        let key_len = read_u16(data, &mut cursor)? as usize;
        let missing = *data
            .get(cursor)
            .ok_or_else(|| anyhow::anyhow!("truncated availability missing bits"))?;
        cursor += 1;
        let last_touch = read_u64(data, &mut cursor)?;
        let inserted_at = read_u64(data, &mut cursor)?;
        let key = data
            .get(cursor..cursor + key_len)
            .ok_or_else(|| anyhow::anyhow!("truncated availability key bytes"))?
            .to_vec();
        cursor += key_len;
        entries.push(PersistedEntry {
            key,
            missing,
            last_touch,
            inserted_at,
        });
    }

    Ok(entries)
}

fn read_u16(data: &[u8], cursor: &mut usize) -> Result<u16> {
    let bytes = data
        .get(*cursor..*cursor + size_of::<u16>())
        .ok_or_else(|| anyhow::anyhow!("truncated u16 field"))?;
    *cursor += size_of::<u16>();
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u64(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let bytes = data
        .get(*cursor..*cursor + size_of::<u64>())
        .ok_or_else(|| anyhow::anyhow!("truncated u64 field"))?;
    *cursor += size_of::<u64>();
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn hash_key(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;

    let mut hasher = XxHash64::default();
    hasher.write(bytes);
    hasher.finish()
}

fn next_power_of_two(value: usize) -> usize {
    value.next_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lookup_miss_returns_none() {
        let index = AvailabilityIndex::new(1024);
        let msg_id = MessageId::from_borrowed("<miss@example.com>").unwrap();
        assert!(index.get(&msg_id).is_none());
    }

    #[test]
    fn record_missing_round_trips_as_negative_cached_article() {
        let index = AvailabilityIndex::new(8192);
        let msg_id = MessageId::from_borrowed("<gone@example.com>").unwrap();
        let backend_id = BackendId::from_index(2);

        index.record_backend_missing(&msg_id, backend_id);

        let cached = index.get(&msg_id).expect("negative entry");
        assert!(cached.has_availability_info());
        assert!(!cached.should_try_backend(backend_id));
        assert_eq!(
            cached.availability().missing_bits(),
            backend_id.availability_bit()
        );
    }

    #[test]
    fn record_missing_expires_after_configured_ttl() {
        let index = AvailabilityIndex::with_ttl(8192, std::time::Duration::from_millis(5));
        let msg_id = MessageId::from_borrowed("<expires@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        assert!(index.get(&msg_id).is_some());

        std::thread::sleep(std::time::Duration::from_millis(15));

        assert!(
            index.get(&msg_id).is_none(),
            "availability-only negatives must expire on the configured cache TTL"
        );
    }

    #[test]
    fn record_missing_supports_highest_backend_bit() {
        let index = AvailabilityIndex::new(8192);
        let msg_id = MessageId::from_borrowed("<highest@example.com>").unwrap();
        let backend_id = BackendId::from_index(7);

        index.record_backend_missing(&msg_id, backend_id);

        let cached = index.get(&msg_id).expect("negative entry");
        assert_eq!(cached.availability().missing_bits(), 0b1000_0000);
        assert!(!cached.should_try_backend(backend_id));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "Backend index 8 exceeds MAX_BACKENDS")]
    fn record_missing_panics_for_out_of_range_backend() {
        let index = AvailabilityIndex::new(8192);
        let msg_id = MessageId::from_borrowed("<panic@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(8));
    }

    #[test]
    fn sync_availability_persists_only_missing_bits() {
        let index = AvailabilityIndex::new(8192);
        let msg_id = MessageId::from_borrowed("<mixed@example.com>").unwrap();
        let mut availability = ArticleAvailability::new();
        availability.record_missing(BackendId::from_index(0));
        availability.record_has(BackendId::from_index(1));

        index.sync_availability(&msg_id, &availability);

        let cached = index.get(&msg_id).expect("negative availability");
        assert_eq!(cached.availability().checked_bits(), 0b0000_0001);
        assert_eq!(cached.availability().missing_bits(), 0b0000_0001);
        assert!(cached.should_try_backend(BackendId::from_index(1)));
    }

    #[test]
    fn zero_capacity_index_never_records_entries() {
        let index = AvailabilityIndex::new(0);
        let msg_id = MessageId::from_borrowed("<nocap@example.com>").unwrap();
        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        assert!(index.get(&msg_id).is_none());
    }

    #[test]
    fn lru_eviction_keeps_newer_entry() {
        let index = AvailabilityIndex::with_shard_count(320, 1);
        let older = MessageId::from_borrowed("<older@example.com>").unwrap();
        let newer = MessageId::from_borrowed("<newer@example.com>").unwrap();
        let latest = MessageId::from_borrowed("<latest@example.com>").unwrap();

        index.record_backend_missing(&older, BackendId::from_index(0));
        index.record_backend_missing(&newer, BackendId::from_index(1));
        assert!(
            index.get(&older).is_some(),
            "touch older so newer becomes LRU"
        );
        index.record_backend_missing(&latest, BackendId::from_index(2));

        assert!(index.get(&latest).is_some());
        assert!(index.get(&older).is_some());
        assert!(
            index.evictions() >= 1,
            "bounded index should evict rather than saturate"
        );
    }

    #[test]
    fn save_and_load_roundtrip_restores_entries() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("availability.idx");
        let index = AvailabilityIndex::with_shard_count(1024, 1);
        let first = MessageId::from_borrowed("<first@example.com>").unwrap();
        let second = MessageId::from_borrowed("<second@example.com>").unwrap();

        index.record_backend_missing(&first, BackendId::from_index(0));
        index.record_backend_missing(&second, BackendId::from_index(2));
        index.save_to_path(&path).unwrap();

        let restored = AvailabilityIndex::with_shard_count(1024, 1);
        assert!(restored.load_from_path(&path).unwrap());

        assert!(
            !restored
                .get(&first)
                .expect("restored first entry")
                .should_try_backend(BackendId::from_index(0))
        );
        assert!(
            !restored
                .get(&second)
                .expect("restored second entry")
                .should_try_backend(BackendId::from_index(2))
        );
    }

    #[test]
    fn load_from_path_skips_expired_entries() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("availability.idx");
        let index = AvailabilityIndex::with_ttl_and_shard_count(
            1024,
            std::time::Duration::from_millis(5),
            1,
        );
        let msg_id = MessageId::from_borrowed("<persisted-expired@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        index.save_to_path(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));

        let restored = AvailabilityIndex::with_ttl_and_shard_count(
            1024,
            std::time::Duration::from_millis(5),
            1,
        );
        assert!(restored.load_from_path(&path).unwrap());

        assert!(
            restored.get(&msg_id).is_none(),
            "persisted availability-only negatives must not outlive cache TTL"
        );
    }
}
