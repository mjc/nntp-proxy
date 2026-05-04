//! Bounded availability-only blocked fingerprint index.
//!
//! Stores negative-only backend availability using a rotating blocked fingerprint
//! filter. The filter is bounded by `capacity_bytes`, favors throughput, and
//! accepts occasional false negatives from rotation/overwrites. False positives
//! are pushed down by storing a keyed 64-bit fingerprint plus a keyed 16-bit
//! confirmation tag per slot.

use super::{ArticleAvailability, CachedArticle, availability::MAX_BACKENDS, ttl};
use crate::io_util::atomic_replace_file;
use crate::types::{BackendId, MessageId};
use anyhow::{Context, Result};
use std::fs;
use std::hash::Hasher;
use std::mem::size_of;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use twox_hash::XxHash64;

const DEFAULT_GENERATIONS: usize = 2;
const BLOCK_SLOTS: usize = 2;
const FIXED_ARTICLE_CAPACITY: usize = 256 * 1024;
const SLOT_BYTES: usize = size_of::<u64>() + size_of::<u16>() + size_of::<u8>();
const ALL_BACKEND_BITS: u8 = ((1u16 << MAX_BACKENDS) - 1) as u8;
const PERSISTENCE_MAGIC: &[u8; 8] = b"ANEGSIM2";
const LEGACY_PERSISTENCE_MAGIC_V1: &[u8; 8] = b"ANEGIDX1";
const LEGACY_PERSISTENCE_MAGIC_V2: &[u8; 8] = b"ANEGIDX2";
const LEGACY_PERSISTENCE_MAGIC_V3: &[u8; 8] = b"ANEGSIM1";

static SAVE_LOCK: Mutex<()> = Mutex::new(());
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
struct Block {
    hashes: [u64; BLOCK_SLOTS],
    tags: [u16; BLOCK_SLOTS],
    missing: [u8; BLOCK_SLOTS],
}

type SlotMatchMask = u8;
const FIXED_TOTAL_BLOCKS: usize = FIXED_ARTICLE_CAPACITY / BLOCK_SLOTS;
const FIXED_CAPACITY_BYTES: u64 = (FIXED_TOTAL_BLOCKS * size_of::<Block>()) as u64;

impl Block {
    fn missing_bits(&self, hash: u64, tag: u16) -> u8 {
        let mut matched = matching_slots(&self.hashes, &self.tags, hash, tag);
        let mut missing_bits = 0u8;

        while matched != 0 {
            let slot = matched.trailing_zeros() as usize;
            missing_bits |= self.missing[slot];
            matched &= matched - 1;
        }

        missing_bits
    }

    fn insert(&mut self, hash: u64, tag: u16, missing_bits: u8, victim: usize) -> InsertOutcome {
        debug_assert_ne!(hash, 0, "fingerprint slots use 0 as the empty sentinel");

        let existing = matching_slots(&self.hashes, &self.tags, hash, tag);
        if existing != 0 {
            let slot = existing.trailing_zeros() as usize;
            self.missing[slot] |= missing_bits;
            return InsertOutcome::Updated;
        }

        if let Some(empty_slot) = self.hashes.iter().position(|&value| value == 0) {
            self.hashes[empty_slot] = hash;
            self.tags[empty_slot] = tag;
            self.missing[empty_slot] = missing_bits;
            return InsertOutcome::Inserted;
        }

        self.hashes[victim] = hash;
        self.tags[victim] = tag;
        self.missing[victim] = missing_bits;
        InsertOutcome::Replaced
    }

    fn clear(&mut self) -> usize {
        let occupied = self.hashes.iter().filter(|&&hash| hash != 0).count();
        self.hashes = [0; BLOCK_SLOTS];
        self.tags = [0; BLOCK_SLOTS];
        self.missing = [0; BLOCK_SLOTS];
        occupied
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertOutcome {
    Inserted,
    Updated,
    Replaced,
}

#[derive(Clone, Debug)]
struct Generation {
    started_at: u64,
    occupied: usize,
    blocks: Box<[Block]>,
}

impl Generation {
    fn new(blocks_per_generation: usize) -> Self {
        Self {
            started_at: 0,
            occupied: 0,
            blocks: vec![Block::default(); blocks_per_generation].into_boxed_slice(),
        }
    }

    fn clear(&mut self) -> usize {
        let evicted = self.occupied;
        for block in &mut self.blocks {
            block.clear();
        }
        self.started_at = 0;
        self.occupied = 0;
        evicted
    }
}

#[derive(Clone, Copy, Debug)]
struct PersistedEntry {
    hash: u64,
    tag: u16,
    missing: u8,
    inserted_at: u64,
}

#[derive(Debug)]
struct FilterState {
    generations: Box<[Generation]>,
    current_generation: usize,
    blocks_per_generation: usize,
    live_generations: usize,
    rotation_interval_millis: u64,
    next_rotation_at: u64,
}

impl FilterState {
    fn new(
        blocks_per_generation: usize,
        generation_count: usize,
        rotation_interval_millis: u64,
    ) -> Self {
        let generation_count = generation_count.max(1);
        Self {
            generations: (0..generation_count)
                .map(|_| Generation::new(blocks_per_generation))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            current_generation: 0,
            blocks_per_generation,
            live_generations: 0,
            rotation_interval_millis,
            next_rotation_at: 0,
        }
    }

    #[cfg(test)]
    fn blocks_per_generation(&self) -> usize {
        self.blocks_per_generation
    }

    #[cfg(test)]
    fn generation_count(&self) -> usize {
        self.generations.len()
    }

    fn active_generation_indices(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.live_generations).map(|offset| {
            (self.current_generation + self.generations.len() - offset) % self.generations.len()
        })
    }

    fn occupied_slots(&self) -> usize {
        self.active_generation_indices()
            .map(|index| self.generations[index].occupied)
            .sum()
    }

    fn reset(&mut self) {
        for generation in &mut self.generations {
            generation.clear();
        }
        self.current_generation = 0;
        self.live_generations = 0;
        self.next_rotation_at = 0;
    }

    fn ensure_current_generation_started(&mut self, now: u64) {
        let generation = &mut self.generations[self.current_generation];
        if generation.started_at != 0 {
            return;
        }

        generation.started_at = now;
        self.live_generations = self.live_generations.max(1);
        self.next_rotation_at = match self.rotation_interval_millis {
            0 | u64::MAX => 0,
            interval => now.saturating_add(interval),
        };
    }

    fn refresh_next_rotation_at(&mut self) {
        self.next_rotation_at = match self.rotation_interval_millis {
            0 | u64::MAX => 0,
            interval => self.generations[self.current_generation]
                .started_at
                .saturating_add(interval),
        };
    }

    fn reanchor_current_generation(&mut self) {
        let Some((index, _)) = self
            .generations
            .iter()
            .enumerate()
            .filter(|(_, generation)| generation.started_at != 0)
            .max_by_key(|(_, generation)| generation.started_at)
        else {
            return;
        };

        self.current_generation = index;
        self.refresh_next_rotation_at();
    }

    fn rotate_if_needed(&mut self, now: u64) -> usize {
        if self.blocks_per_generation == 0 || self.generations.is_empty() {
            return 0;
        }

        let interval = self.rotation_interval_millis;
        if interval == 0 {
            let mut evicted = 0usize;
            for generation in &mut self.generations {
                evicted += generation.clear();
            }
            self.current_generation = 0;
            self.live_generations = 0;
            self.next_rotation_at = 0;
            return evicted;
        }
        if interval == u64::MAX {
            return 0;
        }
        if self.live_generations == 0 || self.next_rotation_at == 0 || now < self.next_rotation_at {
            return 0;
        }

        let scheduled_at = self.next_rotation_at;
        let rotations = (1 + ((now.saturating_sub(self.next_rotation_at)) / interval) as usize)
            .min(self.generations.len());
        let mut evicted = 0usize;
        for step in 0..rotations {
            self.current_generation = (self.current_generation + 1) % self.generations.len();
            let generation = &mut self.generations[self.current_generation];
            evicted += generation.clear();
            generation.started_at =
                scheduled_at.saturating_add((step as u64).saturating_mul(interval));
        }
        self.live_generations = (self.live_generations + rotations).min(self.generations.len());
        self.next_rotation_at =
            scheduled_at.saturating_add((rotations as u64).saturating_mul(interval));
        evicted
    }

    fn lookup_missing_bits(
        &mut self,
        hash: u64,
        tag: u16,
        block_index: usize,
        now: u64,
    ) -> LookupResult {
        let evicted = self.rotate_if_needed(now);
        if self.blocks_per_generation == 0 || self.generations.is_empty() {
            return LookupResult {
                missing_bits: 0,
                evicted,
                empty_after: true,
            };
        }

        let mut missing_bits = 0u8;
        for generation_index in self.active_generation_indices() {
            missing_bits |=
                self.generations[generation_index].blocks[block_index].missing_bits(hash, tag);
            if missing_bits == ALL_BACKEND_BITS {
                break;
            }
        }

        LookupResult {
            missing_bits,
            evicted,
            empty_after: self.occupied_slots() == 0,
        }
    }

    fn insert_missing_bits(
        &mut self,
        hash: u64,
        tag: u16,
        missing_bits: u8,
        block_index: usize,
        victim: usize,
        now: u64,
    ) -> StateInsertOutcome {
        let evicted = self.rotate_if_needed(now);
        if self.blocks_per_generation == 0
            || missing_bits == 0
            || self.rotation_interval_millis == 0
        {
            return StateInsertOutcome {
                changed: false,
                evicted,
            };
        }

        self.ensure_current_generation_started(now);
        let generation = &mut self.generations[self.current_generation];
        let block = &mut generation.blocks[block_index];
        let outcome = block.insert(hash, tag, missing_bits, victim);
        if matches!(outcome, InsertOutcome::Inserted) {
            generation.occupied += 1;
        }

        StateInsertOutcome {
            changed: !matches!(outcome, InsertOutcome::Updated),
            evicted: match outcome {
                InsertOutcome::Replaced => evicted.saturating_add(1),
                _ => evicted,
            },
        }
    }

    fn restore_entry(
        &mut self,
        entry: PersistedEntry,
        now: u64,
        ttl_millis: ttl::CacheTtlMillis,
    ) -> usize {
        if self.blocks_per_generation == 0
            || entry.hash == 0
            || entry.missing == 0
            || ttl::is_expired(
                ttl::CacheTimestampMillis::new(entry.inserted_at),
                ttl_millis,
                ttl::CacheTier::new(0),
            )
        {
            return 0;
        }

        let interval = self.rotation_interval_millis;
        let offset = if interval == 0 || interval == u64::MAX {
            0
        } else {
            (now.saturating_sub(entry.inserted_at) / interval) as usize
        }
        .min(self.generations.len().saturating_sub(1));
        let generation_index =
            (self.current_generation + self.generations.len() - offset) % self.generations.len();
        let generation = &mut self.generations[generation_index];
        let started_at = if interval == 0 || interval == u64::MAX {
            entry.inserted_at
        } else {
            now.saturating_sub((offset as u64).saturating_mul(interval))
        };

        if generation.started_at == 0 {
            generation.started_at = started_at;
        } else {
            generation.started_at = generation.started_at.min(started_at);
        }
        self.live_generations = self.live_generations.max(offset + 1);

        let block = &mut generation.blocks[block_index(entry.hash, self.blocks_per_generation)];
        match block.insert(
            entry.hash,
            entry.tag,
            entry.missing,
            victim_slot(entry.hash),
        ) {
            InsertOutcome::Inserted => {
                generation.occupied += 1;
                0
            }
            InsertOutcome::Updated => 0,
            InsertOutcome::Replaced => 1,
        }
    }

    fn snapshot_entries(&mut self, now: u64) -> SnapshotResult {
        let evicted = self.rotate_if_needed(now);
        let mut entries = Vec::with_capacity(self.occupied_slots());

        for generation_index in self.active_generation_indices() {
            let generation = &self.generations[generation_index];

            for block in &generation.blocks {
                for slot in 0..BLOCK_SLOTS {
                    let hash = block.hashes[slot];
                    let missing = block.missing[slot];
                    if hash == 0 || missing == 0 {
                        continue;
                    }
                    entries.push(PersistedEntry {
                        hash,
                        tag: block.tags[slot],
                        missing,
                        inserted_at: generation.started_at,
                    });
                }
            }
        }

        SnapshotResult { entries, evicted }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LookupResult {
    missing_bits: u8,
    evicted: usize,
    empty_after: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct StateInsertOutcome {
    changed: bool,
    evicted: usize,
}

#[derive(Debug, Default)]
struct SnapshotResult {
    entries: Vec<PersistedEntry>,
    evicted: usize,
}

#[derive(Debug)]
pub struct AvailabilityIndex {
    state: Mutex<FilterState>,
    capacity_bytes: u64,
    has_entries: AtomicBool,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    dropped: AtomicU64,
    evictions: AtomicU64,
    ttl: ttl::CacheTtlMillis,
}

impl Default for AvailabilityIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl AvailabilityIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(Duration::MAX)
    }

    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::with_capacity_and_generation_count(FIXED_CAPACITY_BYTES, ttl, DEFAULT_GENERATIONS)
    }

    #[must_use]
    #[cfg(test)]
    fn with_test_capacity(capacity_bytes: u64) -> Self {
        Self::with_capacity_and_generation_count(capacity_bytes, Duration::MAX, DEFAULT_GENERATIONS)
    }

    #[must_use]
    #[cfg(test)]
    fn with_generation_count(capacity_bytes: u64, generation_count: usize) -> Self {
        Self::with_capacity_and_generation_count(capacity_bytes, Duration::MAX, generation_count)
    }

    #[must_use]
    fn with_capacity_and_generation_count(
        capacity_bytes: u64,
        ttl: Duration,
        generation_count: usize,
    ) -> Self {
        let ttl = ttl::CacheTtlMillis::from_duration(ttl);
        let total_blocks = (capacity_bytes as usize) / size_of::<Block>();
        let generation_count = generation_count.max(1).min(total_blocks.max(1));
        let blocks_per_generation = if total_blocks == 0 {
            0
        } else {
            total_blocks / generation_count
        };
        let rotation_interval_millis = match ttl.get() {
            0 => 0,
            u64::MAX => u64::MAX,
            ttl_millis => (ttl_millis / generation_count as u64).max(1),
        };

        Self {
            state: Mutex::new(FilterState::new(
                blocks_per_generation,
                generation_count,
                rotation_interval_millis,
            )),
            capacity_bytes,
            has_entries: AtomicBool::new(false),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
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
        entries.sort_by_key(|entry| entry.inserted_at);

        let now = ttl::now_millis();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.reset();
        let mut evicted = 0usize;
        for entry in entries {
            evicted += state.restore_entry(entry, now, self.ttl);
        }
        if state.live_generations != 0 {
            state.reanchor_current_generation();
        }
        let has_entries = state.occupied_slots() != 0;
        drop(state);

        self.has_entries.store(has_entries, Ordering::Relaxed);
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.inserts.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.evictions.store(evicted as u64, Ordering::Relaxed);
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

        let now = ttl::now_millis();
        let snapshot = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.snapshot_entries(now)
        };
        if snapshot.evicted != 0 {
            self.evictions
                .fetch_add(snapshot.evicted as u64, Ordering::Relaxed);
        }

        let mut bytes = Vec::with_capacity(
            16 + snapshot.entries.len()
                * (size_of::<u64>() + size_of::<u16>() + 1 + size_of::<u64>()),
        );
        bytes.extend_from_slice(PERSISTENCE_MAGIC);
        bytes.extend_from_slice(&(snapshot.entries.len() as u64).to_le_bytes());
        for entry in snapshot.entries {
            bytes.extend_from_slice(&entry.hash.to_le_bytes());
            bytes.extend_from_slice(&entry.tag.to_le_bytes());
            bytes.push(entry.missing);
            bytes.extend_from_slice(&entry.inserted_at.to_le_bytes());
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
        if !self.has_entries.load(Ordering::Relaxed) {
            return 0;
        }
        let result = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let evicted = state.rotate_if_needed(ttl::now_millis());
            (state.occupied_slots() as u64, evicted)
        };
        self.has_entries.store(result.0 != 0, Ordering::Relaxed);
        if result.1 != 0 {
            self.evictions.fetch_add(result.1 as u64, Ordering::Relaxed);
        }
        result.0
    }

    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.entry_count().saturating_mul(SLOT_BYTES as u64)
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
        if !self.has_entries.load(Ordering::Relaxed) {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let (hash, tag) = hash_key(key.as_bytes());
        let now = ttl::now_millis();
        let lookup = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.blocks_per_generation == 0 {
                LookupResult {
                    missing_bits: 0,
                    evicted: 0,
                    empty_after: true,
                }
            } else {
                let block_index = block_index(hash, state.blocks_per_generation);
                let mut lookup = state.lookup_missing_bits(hash, tag, block_index, now);
                lookup.empty_after = state.occupied_slots() == 0;
                lookup
            }
        };
        if lookup.empty_after {
            self.has_entries.store(false, Ordering::Relaxed);
        }
        if lookup.evicted != 0 {
            self.evictions
                .fetch_add(lookup.evicted as u64, Ordering::Relaxed);
        }

        if lookup.missing_bits != 0 {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(CachedArticle::negative_only(lookup.missing_bits))
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    fn insert_missing_bits(&self, key: &str, missing_bits: u8) {
        if key.is_empty() || missing_bits == 0 || self.ttl.get() == 0 {
            return;
        }

        let (hash, tag) = hash_key(key.as_bytes());
        let outcome = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.blocks_per_generation == 0 {
                StateInsertOutcome::default()
            } else {
                let block_index = block_index(hash, state.blocks_per_generation);
                let victim = victim_slot(hash);
                let now = ttl::now_millis();
                state.insert_missing_bits(hash, tag, missing_bits, block_index, victim, now)
            }
        };

        if outcome.evicted != 0 {
            self.evictions
                .fetch_add(outcome.evicted as u64, Ordering::Relaxed);
        }
        if outcome.changed {
            self.has_entries.store(true, Ordering::Relaxed);
            self.inserts.fetch_add(1, Ordering::Relaxed);
        } else if self.capacity_bytes == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn parse_entries(data: &[u8]) -> Result<Vec<PersistedEntry>> {
    if data.len() < PERSISTENCE_MAGIC.len() + size_of::<u64>() {
        anyhow::bail!("availability index file too short");
    }

    let magic = &data[..PERSISTENCE_MAGIC.len()];
    if magic == LEGACY_PERSISTENCE_MAGIC_V1
        || magic == LEGACY_PERSISTENCE_MAGIC_V2
        || magic == LEGACY_PERSISTENCE_MAGIC_V3
    {
        return Ok(Vec::new());
    }
    if magic != PERSISTENCE_MAGIC {
        anyhow::bail!("unknown availability index format");
    }

    let mut cursor = PERSISTENCE_MAGIC.len();
    let entry_count = read_u64(data, &mut cursor)? as usize;
    let mut entries = Vec::with_capacity(entry_count);

    for _ in 0..entry_count {
        let hash = read_u64(data, &mut cursor)?;
        let tag = read_u16(data, &mut cursor)?;
        let missing = *data
            .get(cursor)
            .ok_or_else(|| anyhow::anyhow!("truncated availability missing bits"))?;
        cursor += 1;
        let inserted_at = read_u64(data, &mut cursor)?;
        entries.push(PersistedEntry {
            hash,
            tag,
            missing,
            inserted_at,
        });
    }

    Ok(entries)
}

fn read_u64(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let bytes = data
        .get(*cursor..*cursor + size_of::<u64>())
        .ok_or_else(|| anyhow::anyhow!("truncated u64 field"))?;
    *cursor += size_of::<u64>();
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u16(data: &[u8], cursor: &mut usize) -> Result<u16> {
    let bytes = data
        .get(*cursor..*cursor + size_of::<u16>())
        .ok_or_else(|| anyhow::anyhow!("truncated u16 field"))?;
    *cursor += size_of::<u16>();
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn hash_key(bytes: &[u8]) -> (u64, u16) {
    let mut primary = XxHash64::default();
    primary.write(bytes);

    let mut tag = XxHash64::with_seed(0x9E37_79B9_7F4A_7C15);
    tag.write(bytes);

    (
        normalize_hash(primary.finish()),
        (tag.finish() & u16::MAX as u64) as u16,
    )
}

fn normalize_hash(hash: u64) -> u64 {
    if hash == 0 { 1 } else { hash }
}

fn block_index(hash: u64, block_count: usize) -> usize {
    debug_assert!(block_count > 0);
    (((hash >> 32) as usize) ^ (hash as usize)) % block_count
}

fn victim_slot(hash: u64) -> usize {
    ((hash >> 48) as usize) & (BLOCK_SLOTS - 1)
}

fn matching_slots(
    hashes: &[u64; BLOCK_SLOTS],
    tags: &[u16; BLOCK_SLOTS],
    needle_hash: u64,
    needle_tag: u16,
) -> SlotMatchMask {
    let mut mask = 0;
    for (index, &value) in hashes.iter().enumerate() {
        if value == needle_hash && tags[index] == needle_tag {
            mask |= 1u8 << index;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_capacity_for(blocks: usize, generations: usize) -> u64 {
        (blocks * generations * size_of::<Block>()) as u64
    }

    #[test]
    fn lookup_miss_returns_none() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
        let msg_id = MessageId::from_borrowed("<miss@example.com>").unwrap();
        assert!(index.get(&msg_id).is_none());
    }

    #[test]
    fn slot_match_requires_confirmation_tag() {
        let mut block = Block::default();
        block.hashes[0] = 42;
        block.tags[0] = 7;
        block.missing[0] = 0b0000_0001;

        assert_eq!(block.missing_bits(42, 7), 0b0000_0001);
        assert_eq!(block.missing_bits(42, 8), 0);
    }

    #[test]
    fn record_missing_round_trips_as_negative_cached_article() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(32, 2));
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
    fn request_message_id_lookup_requires_brackets() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(8, 2));
        let msg_id = MessageId::from_borrowed("<request@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));

        assert!(
            index
                .get_request_message_id("request@example.com")
                .is_none()
        );
        assert!(
            index
                .get_request_message_id("<request@example.com>")
                .is_some()
        );
    }

    #[test]
    fn record_missing_expires_after_configured_ttl() {
        let index = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            std::time::Duration::from_millis(5),
            DEFAULT_GENERATIONS,
        );
        let msg_id = MessageId::from_borrowed("<expires@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        assert!(index.get(&msg_id).is_some());

        std::thread::sleep(std::time::Duration::from_millis(15));

        assert!(index.get(&msg_id).is_none());
    }

    #[test]
    fn record_missing_supports_highest_backend_bit() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
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
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(8, 2));
        let msg_id = MessageId::from_borrowed("<panic@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(8));
    }

    #[test]
    fn sync_availability_persists_only_missing_bits() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
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
        let index = AvailabilityIndex::with_test_capacity(0);
        let msg_id = MessageId::from_borrowed("<nocap@example.com>").unwrap();
        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        assert!(index.get(&msg_id).is_none());
    }

    #[test]
    fn bounded_filter_stays_within_capacity() {
        let capacity = test_capacity_for(4, 2);
        let index = AvailabilityIndex::with_test_capacity(capacity);

        for idx in 0..128 {
            let msg_id = MessageId::new(format!("<bounded-{idx}@example.com>")).unwrap();
            index.record_backend_missing(&msg_id, BackendId::from_index(idx % MAX_BACKENDS));
        }

        assert!(index.used_bytes() <= capacity);
        assert!(index.entry_count() <= (capacity as usize / SLOT_BYTES) as u64);
    }

    #[test]
    fn saturated_block_evicts_old_fingerprints() {
        let capacity = test_capacity_for(1, 1);
        let index = AvailabilityIndex::with_generation_count(capacity, 1);

        for idx in 0..(BLOCK_SLOTS + 4) {
            let msg_id = MessageId::new(format!("<evict-{idx}@example.com>")).unwrap();
            index.record_backend_missing(&msg_id, BackendId::from_index(idx % MAX_BACKENDS));
        }

        let latest = MessageId::new(format!("<evict-{}@example.com>", BLOCK_SLOTS + 3)).unwrap();
        assert!(
            index.get(&latest).is_some(),
            "latest insert should still be resident"
        );
        assert!(
            index.evictions() >= 1,
            "full blocks should overwrite old fingerprints"
        );
        assert!(index.entry_count() <= BLOCK_SLOTS as u64);
    }

    #[test]
    fn save_and_load_roundtrip_restores_entries() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("availability.idx");
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
        let first = MessageId::from_borrowed("<first@example.com>").unwrap();
        let second = MessageId::from_borrowed("<second@example.com>").unwrap();

        index.record_backend_missing(&first, BackendId::from_index(0));
        index.record_backend_missing(&second, BackendId::from_index(2));
        index.save_to_path(&path).unwrap();

        let restored = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
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
    fn save_and_load_roundtrip_restores_multiple_backend_bits() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("availability-multi.idx");
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
        let msg_id = MessageId::from_borrowed("<multi@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(1));
        index.record_backend_missing(&msg_id, BackendId::from_index(3));
        index.save_to_path(&path).unwrap();

        let restored = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
        assert!(restored.load_from_path(&path).unwrap());

        let cached = restored.get(&msg_id).expect("restored negative");
        assert!(!cached.should_try_backend(BackendId::from_index(1)));
        assert!(!cached.should_try_backend(BackendId::from_index(3)));
        assert!(cached.should_try_backend(BackendId::from_index(0)));
    }

    #[test]
    fn load_from_path_skips_expired_entries() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("availability-expired.idx");
        let index = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            std::time::Duration::from_millis(5),
            DEFAULT_GENERATIONS,
        );
        let msg_id = MessageId::from_borrowed("<persisted-expired@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        index.save_to_path(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));

        let restored = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            std::time::Duration::from_millis(5),
            DEFAULT_GENERATIONS,
        );
        assert!(restored.load_from_path(&path).unwrap());
        assert!(restored.get(&msg_id).is_none());
    }

    #[test]
    fn load_from_path_keeps_older_generation_as_rotation_anchor() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("availability-older-generation.idx");
        let index = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            std::time::Duration::from_millis(40),
            DEFAULT_GENERATIONS,
        );
        let msg_id = MessageId::from_borrowed("<persisted-older@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        std::thread::sleep(std::time::Duration::from_millis(25));
        index.save_to_path(&path).unwrap();

        let restored = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            std::time::Duration::from_millis(40),
            DEFAULT_GENERATIONS,
        );
        assert!(restored.load_from_path(&path).unwrap());
        assert!(
            restored.get(&msg_id).is_some(),
            "restored older-generation entry should survive the first lookup"
        );
    }

    #[test]
    fn hit_rate_tracks_hits_and_misses() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(8, 2));
        let hit = MessageId::from_borrowed("<hit-rate@example.com>").unwrap();
        let miss = MessageId::from_borrowed("<miss-rate@example.com>").unwrap();

        index.record_backend_missing(&hit, BackendId::from_index(0));
        assert!(index.get(&hit).is_some());
        assert!(index.get(&miss).is_none());

        assert_eq!(index.hit_rate(), 50.0);
    }

    #[test]
    fn state_uses_expected_geometry() {
        let capacity = test_capacity_for(6, 3);
        let index = AvailabilityIndex::with_generation_count(capacity, 3);
        let state = index.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.generation_count(), 3);
        assert_eq!(state.blocks_per_generation(), 6);
    }
}
