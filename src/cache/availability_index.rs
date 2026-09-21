//! Bounded availability-only blocked fingerprint index.
//!
//! Stores negative-only backend availability using a rotating blocked fingerprint
//! filter. The filter is bounded by `capacity_bytes`, favors throughput, and
//! accepts occasional false negatives from rotation/overwrites. False positives
//! are pushed down by storing a keyed 64-bit fingerprint plus a keyed 16-bit
//! confirmation tag per slot.
//!
//! Article buckets partition one fixed allocation budget across locks; provider
//! identity and fingerprints do not depend on worker identity or shard count.
//! Each shard rotates lazily from its first insert, so generation phases can
//! differ, but the configured retention bound does not increase. Snapshots lock
//! shards in order and keep original observation ages when restored.
//!
//! Until the first recorded fact, a monotonic publication latch permits a
//! hash-free miss. Only these initial-miss statistics use worker-selected
//! stripes; article state is never thread-local. Concurrent metric totals are
//! approximate across shards, and exact when writes have quiesced.

use super::{AvailabilityIdentity, AvailabilityLayout, AvailabilitySlot, MAX_BACKENDS};
use super::{CachedArticle, ttl};
use crate::io_util::atomic_replace_file;
#[cfg(test)]
use crate::types::BackendId;
use crate::types::MessageId;
use anyhow::{Context, Result};
use std::fs;
use std::hash::Hasher;
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use twox_hash::XxHash64;

const DEFAULT_GENERATIONS: usize = 2;
const DEFAULT_SHARDS: usize = 32;
const BLOCK_SLOTS: usize = 2;
const FIXED_ARTICLE_CAPACITY: usize = 256 * 1024;
const ALL_BACKEND_BITS: usize = usize::MAX;
const PERSISTENCE_MAGIC: &[u8; 8] = b"ANEGSIM6";
const LEGACY_PERSISTENCE_MAGIC_V1: &[u8; 8] = b"ANEGIDX1";
const LEGACY_PERSISTENCE_MAGIC_V2: &[u8; 8] = b"ANEGIDX2";
const LEGACY_PERSISTENCE_MAGIC_V3: &[u8; 8] = b"ANEGSIM1";
const LEGACY_PERSISTENCE_MAGIC_V4: &[u8; 8] = b"ANEGSIM2";
const LEGACY_PERSISTENCE_MAGIC_V5: &[u8; 8] = b"ANEGSIM3";
const LEGACY_PERSISTENCE_MAGIC_V6: &[u8; 8] = b"ANEGSIM4";
const LEGACY_PERSISTENCE_MAGIC_V7: &[u8; 8] = b"ANEGSIM5";
const MAX_IDENTITY_FIELD_BYTES: usize = 1024 * 1024;

static SAVE_LOCK: Mutex<()> = Mutex::new(());
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
static NEXT_COUNTER_STRIPE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Only statistics use worker identity; article routing is always key-based.
    static COUNTER_STRIPE: usize = NEXT_COUNTER_STRIPE.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Default)]
struct Block {
    hashes: [u64; BLOCK_SLOTS],
    tags: [u16; BLOCK_SLOTS],
    missing: [usize; BLOCK_SLOTS],
}

type SlotMatchMask = u8;
const FIXED_TOTAL_BLOCKS: usize = FIXED_ARTICLE_CAPACITY / BLOCK_SLOTS;
const FIXED_CAPACITY_BYTES: u64 = (FIXED_TOTAL_BLOCKS * size_of::<Block>()) as u64;

impl Block {
    fn missing_bits(&self, hash: u64, tag: u16) -> usize {
        let mut matched = matching_slots(&self.hashes, &self.tags, hash, tag);
        let mut missing_bits = 0usize;

        while matched != 0 {
            let slot = matched.trailing_zeros() as usize;
            missing_bits |= self.missing[slot];
            matched &= matched - 1;
        }

        missing_bits
    }

    fn insert(&mut self, hash: u64, tag: u16, missing_bits: usize, victim: usize) -> InsertOutcome {
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
struct PersistedEntry<Slots = CurrentProviderBits> {
    hash: u64,
    tag: u16,
    missing: Slots,
    inserted_at: u64,
}

/// Bits in the snapshot's provider table, never the running index's layout.
#[derive(Debug)]
struct StoredProviderBits(usize);

/// Bits produced by the running index or translated into its provider layout.
#[derive(Clone, Copy, Debug)]
struct CurrentProviderBits(usize);

/// Parsed entries retain the table that gives their bit positions meaning.
/// Consuming restoration translates and inserts into the same target index;
/// callers cannot take remapped entries and apply them to another layout.
#[derive(Default)]
struct StoredAvailabilitySnapshot {
    identities: Vec<AvailabilityIdentity>,
    entries: Vec<PersistedEntry<StoredProviderBits>>,
}

impl StoredAvailabilitySnapshot {
    fn restore_into(mut self, index: &AvailabilityIndex) {
        self.entries.sort_by_key(|entry| entry.inserted_at);
        let mut shards = index.lock_all_shards();
        let now = ttl::now_millis();
        for shard in &mut shards {
            shard.filter.reset();
            shard.counters = ShardCounters::default();
        }
        for shard in &index.shards {
            shard.initial_misses.store(0, Ordering::Relaxed);
        }
        for entry in self.entries {
            let missing = self
                .identities
                .iter()
                .enumerate()
                .filter(|(position, _)| entry.missing.0 & (1usize << position) != 0)
                .filter_map(|(_, identity)| index.layout.slot_for_identity(identity))
                .fold(0, |bits, slot| bits | slot.bit());
            let mapped = PersistedEntry {
                hash: entry.hash,
                tag: entry.tag,
                missing: CurrentProviderBits(missing),
                inserted_at: entry.inserted_at,
            };
            let (shard, bucket) = index.locate_hash(entry.hash);
            let state = &mut shards[shard.0];
            state.counters.evictions +=
                state.filter.restore_entry(mapped, bucket, now, index.ttl) as u64;
        }
        for shard in &mut shards {
            if shard.filter.live_generations != 0 {
                shard.filter.reanchor_current_generation();
            }
        }
        if shards
            .iter()
            .any(|shard| shard.filter.occupied_slots() != 0)
        {
            index.populated.get_or_init(|| ());
        }
    }
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
        for _ in 0..rotations {
            self.current_generation = (self.current_generation + 1) % self.generations.len();
            let generation = &mut self.generations[self.current_generation];
            evicted += generation.clear();
        }
        self.live_generations = (self.live_generations + rotations).min(self.generations.len());
        self.next_rotation_at =
            scheduled_at.saturating_add((rotations as u64).saturating_mul(interval));
        evicted
    }

    fn lookup_missing_bits(
        &mut self,
        fingerprint: ArticleFingerprint,
        block_index: LocalBlockIndex,
        now: u64,
    ) -> LookupResult {
        let evicted = self.rotate_if_needed(now);
        if self.blocks_per_generation == 0 || self.generations.is_empty() {
            return LookupResult {
                missing_bits: 0,
                evicted,
            };
        }

        let mut missing_bits = 0usize;
        for generation_index in self.active_generation_indices() {
            missing_bits |= self.generations[generation_index].blocks[block_index.0]
                .missing_bits(fingerprint.hash, fingerprint.tag);
            if missing_bits == ALL_BACKEND_BITS {
                break;
            }
        }

        LookupResult {
            missing_bits,
            evicted,
        }
    }

    fn insert_missing_bits(
        &mut self,
        fingerprint: ArticleFingerprint,
        missing_bits: usize,
        block_index: LocalBlockIndex,
        now: u64,
    ) -> StateInsertOutcome {
        let evicted = self.rotate_if_needed(now);
        if self.blocks_per_generation == 0
            || missing_bits == 0
            || self.rotation_interval_millis == 0
        {
            return StateInsertOutcome { evicted };
        }

        self.ensure_current_generation_started(now);
        let generation = &mut self.generations[self.current_generation];
        let block = &mut generation.blocks[block_index.0];
        let outcome = block.insert(
            fingerprint.hash,
            fingerprint.tag,
            missing_bits,
            victim_slot(fingerprint.hash),
        );
        if matches!(outcome, InsertOutcome::Inserted) {
            generation.occupied += 1;
        }

        StateInsertOutcome {
            evicted: match outcome {
                InsertOutcome::Replaced => evicted.saturating_add(1),
                _ => evicted,
            },
        }
    }

    fn restore_entry(
        &mut self,
        entry: PersistedEntry,
        block_index: LocalBlockIndex,
        now: u64,
        ttl_millis: ttl::CacheTtlMillis,
    ) -> usize {
        if self.blocks_per_generation == 0
            || entry.hash == 0
            || entry.missing.0 == 0
            || now.saturating_sub(entry.inserted_at) >= ttl_millis.get()
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
        // Restore must not turn historical 430 evidence into a fresh observation.
        let started_at = entry.inserted_at;

        if generation.started_at == 0 {
            generation.started_at = started_at;
        } else {
            generation.started_at = generation.started_at.min(started_at);
        }
        self.live_generations = self.live_generations.max(offset + 1);

        let block = &mut generation.blocks[block_index.0];
        match block.insert(
            entry.hash,
            entry.tag,
            entry.missing.0,
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
                        missing: CurrentProviderBits(missing),
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
    missing_bits: usize,
    evicted: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct StateInsertOutcome {
    evicted: usize,
}

#[derive(Debug, Default)]
struct SnapshotResult {
    entries: Vec<PersistedEntry>,
    evicted: usize,
}

#[derive(Debug)]
pub struct AvailabilityIndex {
    shards: Box<[AvailabilityShard]>,
    // Monotonic: once a write has completed, never bypass the locked lookup.
    // In particular, expiry/metrics cannot race a write by clearing this state.
    populated: OnceLock<()>,
    blocks_per_generation: usize,
    layout: AvailabilityLayout,
    capacity_bytes: u64,
    ttl: ttl::CacheTtlMillis,
}

#[derive(Debug)]
struct AvailabilityShard {
    state: Mutex<ShardState>,
    initial_misses: AtomicU64,
}

impl AvailabilityShard {
    fn lock(&self) -> std::sync::LockResult<MutexGuard<'_, ShardState>> {
        self.state.lock()
    }
}

#[derive(Debug, Default)]
struct ShardCounters {
    hits: u64,
    misses: u64,
    evictions: u64,
}

#[derive(Debug)]
struct ShardState {
    filter: FilterState,
    counters: ShardCounters,
}

#[derive(Clone, Copy)]
struct ArticleFingerprint {
    hash: u64,
    tag: u16,
}

impl ArticleFingerprint {
    fn from_key(key: &str) -> Self {
        let (hash, tag) = hash_key(key.as_bytes());
        Self { hash, tag }
    }
}

/// Bucket in the original, unpartitioned generation.
struct GlobalBlockIndex(usize);
struct ShardIndex(usize);
/// Bucket relative to a single shard's generation allocation.
#[derive(Clone, Copy)]
struct LocalBlockIndex(usize);

#[cfg(response_contract)]
const _: fn() = || {
    let mut filter = FilterState::new(1, 2, u64::MAX);
    #[cfg(not(response_contract = "availability_coordinate"))]
    let coordinate = LocalBlockIndex(0);
    #[cfg(response_contract = "availability_coordinate")]
    let coordinate = GlobalBlockIndex(0);
    filter.lookup_missing_bits(ArticleFingerprint { hash: 1, tag: 2 }, coordinate, 1);
};

/// Routing retains the selected resource; no caller pairs a shard with an
/// independently computed local bucket or fingerprint.
struct ArticlePartition<'a> {
    shard: &'a AvailabilityShard,
    fingerprint: ArticleFingerprint,
    bucket: LocalBlockIndex,
}

impl ArticlePartition<'_> {
    fn lookup(self, now: u64) -> Option<CachedArticle> {
        let mut shard = self
            .shard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = shard
            .filter
            .lookup_missing_bits(self.fingerprint, self.bucket, now);
        shard.counters.evictions += result.evicted as u64;
        if result.missing_bits == 0 {
            shard.counters.misses += 1;
            drop(shard);
            None
        } else {
            shard.counters.hits += 1;
            drop(shard);
            Some(CachedArticle::negative_only(result.missing_bits))
        }
    }

    fn record_missing(self, bits: CurrentProviderBits, now: u64) {
        let mut shard = self
            .shard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = shard
            .filter
            .insert_missing_bits(self.fingerprint, bits.0, self.bucket, now);
        shard.counters.evictions += result.evicted as u64;
    }
}

impl Default for AvailabilityIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl AvailabilityIndex {
    #[cfg(test)]
    pub fn record_backend_missing(&self, message_id: &MessageId<'_>, backend_id: BackendId) {
        let slot = AvailabilitySlot::new(backend_id.as_index()).expect("backend count fits bitmap");
        self.record_availability_missing(message_id, slot);
    }

    #[must_use]
    pub const fn fixed_capacity_bytes() -> u64 {
        FIXED_CAPACITY_BYTES
    }

    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(Duration::MAX)
    }

    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::with_capacity_and_generation_count(FIXED_CAPACITY_BYTES, ttl, DEFAULT_GENERATIONS)
    }

    #[cfg(feature = "framing-bench")]
    pub(crate) fn with_benchmark_shards(ttl: Duration, shards: usize) -> Self {
        Self::with_geometry(FIXED_CAPACITY_BYTES, ttl, DEFAULT_GENERATIONS, shards)
    }

    #[must_use]
    pub(crate) fn with_layout(ttl: Duration, layout: AvailabilityLayout) -> Self {
        let mut index = Self::with_ttl(ttl);
        index.layout = layout;
        index
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
        Self::with_geometry(capacity_bytes, ttl, generation_count, DEFAULT_SHARDS)
    }

    fn with_geometry(
        capacity_bytes: u64,
        ttl: Duration,
        generation_count: usize,
        shard_count: usize,
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

        let shard_count = shard_count.max(1).min(blocks_per_generation.max(1));
        let shards = (0..shard_count)
            .map(|shard| AvailabilityShard {
                state: Mutex::new(ShardState {
                    filter: FilterState::new(
                        blocks_per_generation / shard_count
                            + usize::from(shard < blocks_per_generation % shard_count),
                        generation_count,
                        rotation_interval_millis,
                    ),
                    counters: ShardCounters::default(),
                }),
                initial_misses: AtomicU64::new(0),
            })
            .collect();
        Self {
            shards,
            populated: OnceLock::new(),
            blocks_per_generation,
            layout: AvailabilityLayout::synthetic(MAX_BACKENDS),
            capacity_bytes,
            ttl,
        }
    }

    fn locate_bucket(&self, global: GlobalBlockIndex) -> (ShardIndex, LocalBlockIndex) {
        (
            ShardIndex(global.0 % self.shards.len()),
            LocalBlockIndex(global.0 / self.shards.len()),
        )
    }

    fn locate_hash(&self, hash: u64) -> (ShardIndex, LocalBlockIndex) {
        let global = if self.blocks_per_generation == 0 {
            0
        } else {
            block_index(hash, self.blocks_per_generation)
        };
        self.locate_bucket(GlobalBlockIndex(global))
    }

    fn partition(&self, fingerprint: ArticleFingerprint) -> ArticlePartition<'_> {
        let (shard, bucket) = self.locate_hash(fingerprint.hash);
        ArticlePartition {
            shard: &self.shards[shard.0],
            fingerprint,
            bucket,
        }
    }

    /// Only cold snapshot/restore operations hold more than one shard, always
    /// in allocation order and never during filesystem I/O.
    fn lock_all_shards(&self) -> Vec<MutexGuard<'_, ShardState>> {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect()
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

    pub(crate) fn record_availability_missing(
        &self,
        message_id: &MessageId<'_>,
        slot: AvailabilitySlot,
    ) {
        self.insert_missing_bits(message_id.without_brackets(), slot.bit());
    }

    pub fn load_from_path(&self, path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }

        let data = fs::read(path).with_context(|| {
            format!("Failed to read availability index from {}", path.display())
        })?;
        parse_snapshot(&data)?
            .unwrap_or_default()
            .restore_into(self);
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

        let snapshot = self.snapshot_entries();

        let mut bytes = Vec::with_capacity(
            16 + snapshot.entries.len()
                * (size_of::<u64>() + size_of::<u16>() + size_of::<u64>() + size_of::<u64>()),
        );
        bytes.extend_from_slice(PERSISTENCE_MAGIC);
        bytes.extend_from_slice(&(self.layout.identity_count() as u64).to_le_bytes());
        for identity in self.layout.identities() {
            write_identity(&mut bytes, identity)?;
        }
        bytes.extend_from_slice(&(snapshot.entries.len() as u64).to_le_bytes());
        for entry in snapshot.entries {
            bytes.extend_from_slice(&entry.hash.to_le_bytes());
            bytes.extend_from_slice(&entry.tag.to_le_bytes());
            bytes.extend_from_slice(&availability_bits_to_wire(entry.missing.0)?.to_le_bytes());
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

    fn snapshot_entries(&self) -> SnapshotResult {
        let mut shards = self.lock_all_shards();
        let now = ttl::now_millis();
        let mut snapshot = SnapshotResult::default();
        for shard in &mut shards {
            let mut part = shard.filter.snapshot_entries(now);
            shard.counters.evictions += part.evicted as u64;
            snapshot.entries.append(&mut part.entries);
            snapshot.evicted += part.evicted;
        }
        snapshot
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.shards
            .iter()
            .map(|shard| {
                let mut shard = shard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                shard.counters.evictions += shard.filter.rotate_if_needed(ttl::now_millis()) as u64;
                shard.filter.occupied_slots() as u64
            })
            .sum()
    }

    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let (hits, misses) = self.shards.iter().fold((0, 0), |(hits, misses), shard| {
            let initial_misses = shard.initial_misses.load(Ordering::Relaxed);
            let shard = shard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                hits + shard.counters.hits,
                misses + shard.counters.misses + initial_misses,
            )
        });
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            (hits as f64 / total as f64) * 100.0
        }
    }

    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .counters
                    .evictions
            })
            .sum()
    }

    fn lookup_by_key(&self, key: &str) -> Option<CachedArticle> {
        if self.populated.get().is_none() {
            COUNTER_STRIPE.with(|stripe| {
                self.shards[stripe % self.shards.len()]
                    .initial_misses
                    .fetch_add(1, Ordering::Relaxed);
            });
            return None;
        }
        self.partition(ArticleFingerprint::from_key(key))
            .lookup(ttl::now_millis())
    }

    fn insert_missing_bits(&self, key: &str, missing_bits: usize) {
        if key.is_empty() || missing_bits == 0 || self.ttl.get() == 0 {
            return;
        }

        self.partition(ArticleFingerprint::from_key(key))
            .record_missing(CurrentProviderBits(missing_bits), ttl::now_millis());
        self.populated.get_or_init(|| ());
    }
}

fn parse_snapshot(data: &[u8]) -> Result<Option<StoredAvailabilitySnapshot>> {
    if data.len() < PERSISTENCE_MAGIC.len() + size_of::<u64>() {
        anyhow::bail!("availability index file too short");
    }

    let magic = &data[..PERSISTENCE_MAGIC.len()];
    if magic == LEGACY_PERSISTENCE_MAGIC_V1
        || magic == LEGACY_PERSISTENCE_MAGIC_V2
        || magic == LEGACY_PERSISTENCE_MAGIC_V3
        || magic == LEGACY_PERSISTENCE_MAGIC_V4
        || magic == LEGACY_PERSISTENCE_MAGIC_V5
        || magic == LEGACY_PERSISTENCE_MAGIC_V6
        || magic == LEGACY_PERSISTENCE_MAGIC_V7
    {
        return Ok(None);
    }
    if magic != PERSISTENCE_MAGIC {
        anyhow::bail!("unknown availability index format");
    }

    let mut cursor = PERSISTENCE_MAGIC.len();
    let identity_count = read_u64(data, &mut cursor)?;
    let identity_count = usize::try_from(identity_count)
        .ok()
        .filter(|count| *count <= MAX_BACKENDS)
        .ok_or_else(|| anyhow::anyhow!("invalid availability identity count"))?;
    let mut identities = Vec::with_capacity(identity_count);
    for _ in 0..identity_count {
        identities.push(read_identity(data, &mut cursor)?);
    }
    if identities
        .iter()
        .enumerate()
        .any(|(index, identity)| identities[..index].contains(identity))
    {
        anyhow::bail!("duplicate availability identity");
    }
    let entry_count = usize::try_from(read_u64(data, &mut cursor)?)?;
    let entry_width = size_of::<u64>() + size_of::<u16>() + size_of::<u64>() + size_of::<u64>();
    if entry_count > data.len().saturating_sub(cursor) / entry_width {
        anyhow::bail!("invalid availability entry count");
    }
    let mut entries = Vec::with_capacity(entry_count);

    for _ in 0..entry_count {
        let hash = read_u64(data, &mut cursor)?;
        let tag = read_u16(data, &mut cursor)?;
        let missing_wire = read_u64(data, &mut cursor)?;
        let identity_mask = if identity_count == usize::BITS as usize {
            usize::MAX
        } else {
            (1usize << identity_count).wrapping_sub(1)
        };
        if missing_wire & !(identity_mask as u64) != 0 {
            anyhow::bail!("availability bits exceed declared identity table");
        }
        let missing = StoredProviderBits(availability_bits_from_wire(missing_wire)?);
        let inserted_at = read_u64(data, &mut cursor)?;
        entries.push(PersistedEntry {
            hash,
            tag,
            missing,
            inserted_at,
        });
    }

    if cursor != data.len() {
        anyhow::bail!("trailing bytes in availability index");
    }

    Ok(Some(StoredAvailabilitySnapshot {
        identities,
        entries,
    }))
}

fn write_identity(bytes: &mut Vec<u8>, identity: &AvailabilityIdentity) -> Result<()> {
    let host = identity.as_host().as_bytes();
    let host_len = u32::try_from(host.len()).context("availability host too long")?;
    bytes.extend_from_slice(&host_len.to_le_bytes());
    bytes.extend_from_slice(host);
    Ok(())
}

fn read_identity(data: &[u8], cursor: &mut usize) -> Result<AvailabilityIdentity> {
    AvailabilityIdentity::from_persisted_host(read_string(data, cursor, "host")?)
}

fn read_string(data: &[u8], cursor: &mut usize, field: &str) -> Result<String> {
    let length = usize::try_from(read_u32(data, cursor)?)?;
    if length > MAX_IDENTITY_FIELD_BYTES {
        anyhow::bail!("availability {field} is too long");
    }
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("availability {field} length overflow"))?;
    let bytes = data
        .get(*cursor..end)
        .ok_or_else(|| anyhow::anyhow!("truncated availability {field}"))?;
    *cursor = end;
    let value = String::from_utf8(bytes.to_vec())
        .with_context(|| format!("invalid availability {field}"))?;
    if value.is_empty() {
        anyhow::bail!("empty availability {field}");
    }
    Ok(value)
}

fn read_u32(data: &[u8], cursor: &mut usize) -> Result<u32> {
    let bytes = data
        .get(*cursor..*cursor + size_of::<u32>())
        .ok_or_else(|| anyhow::anyhow!("truncated u32 field"))?;
    *cursor += size_of::<u32>();
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
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

fn availability_bits_to_wire(bits: usize) -> Result<u64> {
    u64::try_from(bits).context("availability bitmap exceeds u64 wire format")
}

fn availability_bits_from_wire(bits: u64) -> Result<usize> {
    usize::try_from(bits).context("availability bitmap exceeds usize on this target")
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
    use super::super::availability::MAX_BACKENDS;
    use super::*;
    use tempfile::TempDir;

    fn test_capacity_for(blocks: usize, generations: usize) -> u64 {
        (blocks * generations * size_of::<Block>()) as u64
    }

    #[test]
    fn restoring_an_observation_preserves_its_original_age() {
        let now = ttl::now_millis();
        let inserted_at = now - 25;
        let mut filter = FilterState::new(1, 2, 100);
        filter.restore_entry(
            PersistedEntry {
                hash: 42,
                tag: 7,
                missing: CurrentProviderBits(1),
                inserted_at,
            },
            LocalBlockIndex(0),
            now,
            ttl::CacheTtlMillis::new(200),
        );
        filter.reanchor_current_generation();
        assert_eq!(
            filter.snapshot_entries(now).entries[0].inserted_at,
            inserted_at
        );
        assert_eq!(
            filter
                .lookup_missing_bits(
                    ArticleFingerprint { hash: 42, tag: 7 },
                    LocalBlockIndex(0),
                    inserted_at + 200
                )
                .missing_bits,
            0
        );
    }

    #[test]
    fn partitions_preserve_bucket_geometry_and_total_capacity() {
        for blocks in 0..35 {
            for requested in [1, 4, 8, 16, 32] {
                let index = AvailabilityIndex::with_geometry(
                    test_capacity_for(blocks, 2),
                    Duration::MAX,
                    2,
                    requested,
                );
                let actual_blocks: usize = index
                    .shards
                    .iter()
                    .map(|shard| shard.lock().unwrap().filter.blocks_per_generation)
                    .sum();
                assert_eq!(actual_blocks, blocks);
                let mut visited = std::collections::HashSet::new();
                for bucket in 0..blocks {
                    let (shard, local) = index.locate_bucket(GlobalBlockIndex(bucket));
                    assert!(
                        local.0
                            < index.shards[shard.0]
                                .lock()
                                .unwrap()
                                .filter
                                .blocks_per_generation
                    );
                    assert!(visited.insert((shard.0, local.0)));
                }
                assert_eq!(visited.len(), blocks);
            }
        }
    }

    #[test]
    fn partitions_merge_concurrent_provider_facts() {
        let index = AvailabilityIndex::new();
        let id = MessageId::from_borrowed("<concurrent@example.com>").unwrap();
        std::thread::scope(|scope| {
            for provider in 0..16 {
                let index = &index;
                let id = &id;
                scope.spawn(move || {
                    index.record_availability_missing(id, AvailabilitySlot::new(provider).unwrap())
                });
            }
        });
        assert_eq!(
            index.get(&id).unwrap().availability().missing_bits(),
            0xffff
        );
    }

    #[test]
    fn a_locked_partition_does_not_block_another_article_partition() {
        let index = AvailabilityIndex::new();
        let fingerprint = (1..1000)
            .map(|hash| ArticleFingerprint { hash, tag: 1 })
            .find(|fingerprint| index.locate_hash(fingerprint.hash).0.0 != 0)
            .unwrap();
        let held = index.shards[0].lock().unwrap();
        std::thread::scope(|scope| {
            let (send, receive) = std::sync::mpsc::channel();
            let index = &index;
            scope.spawn(move || {
                index
                    .partition(fingerprint)
                    .record_missing(CurrentProviderBits(1), 10_000);
                send.send(index.partition(fingerprint).lookup(10_000).is_some())
                    .unwrap();
            });
            let result = receive.recv_timeout(Duration::from_secs(5));
            drop(held);
            assert!(result.unwrap());
        });
    }

    #[test]
    fn partition_expiry_does_not_depend_on_other_partition_activity() {
        let fingerprint = ArticleFingerprint { hash: 42, tag: 7 };
        for count in [1, 4, 16] {
            let index = AvailabilityIndex::with_geometry(
                FIXED_CAPACITY_BYTES,
                Duration::from_millis(200),
                2,
                count,
            );
            index
                .partition(fingerprint)
                .record_missing(CurrentProviderBits(1), 10_000);
            assert!(index.partition(fingerprint).lookup(10_199).is_some());
            assert!(index.partition(fingerprint).lookup(10_200).is_none());
            assert!(index.partition(fingerprint).lookup(100_000).is_none());
            index
                .partition(fingerprint)
                .record_missing(CurrentProviderBits(2), 100_000);
            assert_eq!(
                index
                    .partition(fingerprint)
                    .lookup(100_000)
                    .unwrap()
                    .availability()
                    .missing_bits(),
                2
            );
            assert_eq!(index.evictions(), 1);
        }
    }

    #[test]
    fn snapshot_and_metrics_can_run_alongside_provider_updates() {
        let index = AvailabilityIndex::new();
        let id = MessageId::from_borrowed("<snapshot-race@test>").unwrap();
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("concurrent.idx");
        std::thread::scope(|scope| {
            for provider in 0..8 {
                let index = &index;
                let id = &id;
                scope.spawn(move || {
                    for _ in 0..100 {
                        index.record_availability_missing(
                            id,
                            AvailabilitySlot::new(provider).unwrap(),
                        );
                        assert!(index.get(id).is_some());
                    }
                });
            }
            for _ in 0..4 {
                index.save_to_path(&path).unwrap();
                assert!(index.entry_count() <= 1);
                assert!(index.hit_rate() <= 100.0);
            }
        });
        index.save_to_path(&path).unwrap();
        let restored = AvailabilityIndex::new();
        restored.load_from_path(&path).unwrap();
        assert_eq!(
            restored.get(&id).unwrap().availability().missing_bits(),
            255
        );
        assert_eq!(index.hit_rate(), 100.0);
        assert_eq!(index.evictions(), 0);
    }

    #[test]
    fn snapshots_restore_across_partition_counts() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("partitioned.idx");
        let ids: Vec<_> = (0..64)
            .map(|i| MessageId::new(format!("<partition-{i}@test>")).unwrap())
            .collect();
        let source = AvailabilityIndex::with_geometry(FIXED_CAPACITY_BYTES, Duration::MAX, 2, 1);
        for id in &ids {
            source.record_availability_missing(id, AvailabilitySlot::new(3).unwrap());
        }
        source.save_to_path(&path).unwrap();
        for count in [4, 16, 1] {
            let restored =
                AvailabilityIndex::with_geometry(FIXED_CAPACITY_BYTES, Duration::MAX, 2, count);
            restored.load_from_path(&path).unwrap();
            for id in &ids {
                assert!(
                    !restored
                        .get(id)
                        .unwrap()
                        .should_try_slot(AvailabilitySlot::new(3).unwrap())
                );
            }
            restored.save_to_path(&path).unwrap();
        }
    }

    #[test]
    fn partitioning_preserves_collision_victims_at_equal_capacity() {
        let capacity = test_capacity_for(67, 2);
        let single = AvailabilityIndex::with_geometry(capacity, Duration::MAX, 2, 1);
        let partitioned = AvailabilityIndex::with_geometry(capacity, Duration::MAX, 2, 32);
        let ids: Vec<_> = (0..4096)
            .map(|i| MessageId::new(format!("<collision-{i}@test>")).unwrap())
            .collect();
        for (i, id) in ids.iter().enumerate() {
            let slot = AvailabilitySlot::new(i % 8).unwrap();
            single.record_availability_missing(id, slot);
            partitioned.record_availability_missing(id, slot);
        }
        for id in &ids {
            let bits = |index: &AvailabilityIndex| {
                index
                    .get(id)
                    .map(|article| article.availability().missing_bits())
            };
            assert_eq!(bits(&single), bits(&partitioned));
        }
        assert_eq!(single.entry_count(), partitioned.entry_count());
        assert_eq!(single.evictions(), partitioned.evictions());
    }

    #[test]
    fn cold_miss_stripes_remain_visible_in_metrics_after_first_insert() {
        let index = AvailabilityIndex::new();
        let id = MessageId::from_borrowed("<cold-stats@test>").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let index = &index;
                let id = &id;
                scope.spawn(move || assert!(index.get(id).is_none()));
            }
        });
        index.record_availability_missing(&id, AvailabilitySlot::new(0).unwrap());
        assert!(index.get(&id).is_some());
        assert_eq!(index.hit_rate(), 100.0 / 9.0);
    }

    fn rewrite_persisted_inserted_at(path: &std::path::Path, inserted_at: u64) {
        let data = std::fs::read(path).unwrap();
        let StoredAvailabilitySnapshot {
            identities,
            mut entries,
        } = parse_snapshot(&data).unwrap().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "test helper expects exactly one persisted entry"
        );
        entries[0].inserted_at = inserted_at;

        let mut bytes = Vec::with_capacity(data.len());
        bytes.extend_from_slice(PERSISTENCE_MAGIC);
        bytes.extend_from_slice(&(identities.len() as u64).to_le_bytes());
        for identity in &identities {
            write_identity(&mut bytes, identity).unwrap();
        }
        bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry.hash.to_le_bytes());
            bytes.extend_from_slice(&entry.tag.to_le_bytes());
            bytes.extend_from_slice(
                &availability_bits_to_wire(entry.missing.0)
                    .unwrap()
                    .to_le_bytes(),
            );
            bytes.extend_from_slice(&entry.inserted_at.to_le_bytes());
        }

        std::fs::write(path, bytes).unwrap();
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
    fn persisted_missing_bits_follow_identity_when_servers_reorder() {
        let first = [
            crate::config::Server::builder(
                "news-a.example",
                crate::types::Port::try_new(119).unwrap(),
            )
            .build()
            .unwrap(),
            crate::config::Server::builder(
                "news-b.example",
                crate::types::Port::try_new(119).unwrap(),
            )
            .build()
            .unwrap(),
        ];
        let reordered = [first[1].clone(), first[0].clone()];
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("availability.idx");
        let original = AvailabilityIndex::with_layout(
            Duration::MAX,
            AvailabilityLayout::from_servers(&first).unwrap(),
        );
        let b = BackendId::from_index(1);
        let msg_id = MessageId::from_borrowed("<reorder@example.com>").unwrap();
        original.record_availability_missing(
            &msg_id,
            AvailabilityLayout::from_servers(&first)
                .unwrap()
                .slot_for_backend(b),
        );
        original.save_to_path(&path).unwrap();
        let persisted = std::fs::read(&path).unwrap();
        assert!(
            !persisted
                .windows(b"first-secret".len())
                .any(|window| window == b"first-secret")
        );

        let restored = AvailabilityIndex::with_layout(
            Duration::MAX,
            AvailabilityLayout::from_servers(&reordered).unwrap(),
        );
        restored.load_from_path(&path).unwrap();
        let cached = restored.get(&msg_id).unwrap();
        assert_eq!(cached.availability().missing_bits(), 0b01);
    }

    #[test]
    fn snapshot_restore_discards_removed_providers_and_shares_accounts() {
        let server = |host, username| {
            crate::config::Server::builder(host, crate::types::Port::try_new(119).unwrap())
                .username(username)
                .password("secret")
                .build()
                .unwrap()
        };
        let original = AvailabilityIndex::with_layout(
            Duration::MAX,
            AvailabilityLayout::from_servers(&[
                server("removed.example", "first"),
                server("retained.example", "first"),
            ])
            .unwrap(),
        );
        let message = MessageId::from_borrowed("<restore@example.com>").unwrap();
        original.record_availability_missing(
            &message,
            original.layout.slot_for_backend(BackendId::from_index(0)),
        );
        original.record_availability_missing(
            &message,
            original.layout.slot_for_backend(BackendId::from_index(1)),
        );
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("availability.idx");
        original.save_to_path(&path).unwrap();
        let target = AvailabilityIndex::with_layout(
            Duration::MAX,
            AvailabilityLayout::from_servers(&[
                server("retained.example", "second"),
                server("retained.example", "third"),
                server("new.example", "first"),
            ])
            .unwrap(),
        );
        parse_snapshot(&fs::read(path).unwrap())
            .unwrap()
            .unwrap()
            .restore_into(&target);
        assert_eq!(
            target.get(&message).unwrap().availability().missing_bits(),
            1
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
    fn record_missing_supports_backend_eight_bit() {
        let index = AvailabilityIndex::with_test_capacity(test_capacity_for(16, 2));
        let msg_id = MessageId::from_borrowed("<highest@example.com>").unwrap();
        let backend_id = BackendId::from_index(8);

        index.record_backend_missing(&msg_id, backend_id);

        let cached = index.get(&msg_id).expect("negative entry");
        assert_eq!(cached.availability().missing_bits(), 0b1_0000_0000);
        assert!(!cached.should_try_backend(backend_id));
    }

    #[test]
    #[cfg(debug_assertions)]
    fn record_missing_cannot_receive_out_of_range_backend() {
        assert!(BackendId::try_from_index(usize::BITS as usize).is_none());
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
        let slot_bytes = size_of::<u64>() + size_of::<u16>() + size_of::<u8>();
        assert!(index.entry_count() <= (capacity as usize / slot_bytes) as u64);
    }

    #[test]
    fn used_bytes_reports_preallocated_capacity() {
        let capacity = test_capacity_for(4, 2);
        let index = AvailabilityIndex::with_test_capacity(capacity);
        let msg_id = MessageId::from_borrowed("<allocated@example.com>").unwrap();

        assert_eq!(index.used_bytes(), capacity);

        index.record_backend_missing(&msg_id, BackendId::from_index(0));

        assert_eq!(index.used_bytes(), capacity);
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
        let ttl = std::time::Duration::from_millis(200);
        let index = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            ttl,
            DEFAULT_GENERATIONS,
        );
        let msg_id = MessageId::from_borrowed("<persisted-older@example.com>").unwrap();

        index.record_backend_missing(&msg_id, BackendId::from_index(0));
        index.save_to_path(&path).unwrap();
        rewrite_persisted_inserted_at(
            &path,
            ttl::now_millis().saturating_sub((ttl.as_millis() as u64 / 2) + 20),
        );

        let restored = AvailabilityIndex::with_capacity_and_generation_count(
            test_capacity_for(8, 2),
            ttl,
            DEFAULT_GENERATIONS,
        );
        assert!(restored.load_from_path(&path).unwrap());
        assert!(
            restored.get(&msg_id).is_some(),
            "restored older-generation entry should survive the first lookup"
        );
    }

    #[test]
    fn rotated_generation_starts_when_it_receives_a_new_insert() {
        let ttl = std::time::Duration::from_millis(200);
        let index =
            AvailabilityIndex::with_geometry(test_capacity_for(8, 2), ttl, DEFAULT_GENERATIONS, 1);
        let first = MessageId::from_borrowed("<before-rotate@example.com>").unwrap();
        let second = MessageId::from_borrowed("<after-rotate@example.com>").unwrap();
        let forced_old_started_at = ttl::now_millis().saturating_sub(50);

        index.record_backend_missing(&first, BackendId::from_index(0));
        {
            let mut shard = index.shards[0].lock().unwrap();
            let state = &mut shard.filter;
            let current_generation = state.current_generation;
            state.generations[current_generation].started_at = forced_old_started_at;
            state.next_rotation_at = ttl::now_millis().saturating_sub(1);
        }
        let before_second_insert = ttl::now_millis();
        index.record_backend_missing(&second, BackendId::from_index(1));

        let shard = index.shards[0].lock().unwrap();
        let state = &shard.filter;
        let current_generation = &state.generations[state.current_generation];
        assert!(
            current_generation.started_at >= before_second_insert,
            "rotated generation should start when the new insert arrives"
        );
        assert!(
            current_generation.started_at > forced_old_started_at,
            "rotated generation should not keep the historical rotation timestamp"
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
        let shards = index.lock_all_shards();
        assert!(
            shards
                .iter()
                .all(|shard| shard.filter.generation_count() == 3)
        );
        assert_eq!(
            shards
                .iter()
                .map(|shard| shard.filter.blocks_per_generation())
                .sum::<usize>(),
            6
        );
    }
}
