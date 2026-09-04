//! Stable article-availability identities, separate from transport backends.

use crate::config::Server;
use crate::types::BackendId;
use anyhow::{Context, Result};
use std::fmt;
use std::fs;
use std::hash::Hasher;
use std::path::Path;
use twox_hash::XxHash64;

const REGISTRY_MAGIC: &[u8; 8] = b"ANEGREG1";
const MAX_REGISTRY_FIELD_BYTES: usize = 1024 * 1024;

/// A backend namespace whose authoritative article facts may be shared.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AvailabilityIdentity {
    pub(crate) host: String,
    pub(crate) username: Option<String>,
}

impl AvailabilityIdentity {
    #[must_use]
    pub(crate) fn from_server(server: &Server) -> Self {
        Self {
            host: server.host.to_string(),
            username: server.username.clone(),
        }
    }
}

/// Bit position used by article availability state.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AvailabilitySlot(usize);

impl AvailabilitySlot {
    #[must_use]
    pub(crate) const fn new(index: usize) -> Option<Self> {
        if index < usize::BITS as usize {
            Some(Self(index))
        } else {
            None
        }
    }

    #[must_use]
    pub(crate) const fn bit(self) -> usize {
        1usize << self.0
    }
}

impl fmt::Display for AvailabilitySlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Immutable mapping from configured transport backends to article namespaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AvailabilityLayout {
    backend_slots: Box<[AvailabilitySlot]>,
    identities: Box<[AvailabilityIdentity]>,
    mask: usize,
    epoch: u64,
}

impl AvailabilityLayout {
    #[must_use]
    pub(crate) fn synthetic(count: usize) -> Self {
        let identities = (0..count)
            .map(|index| AvailabilityIdentity {
                host: format!("backend-{index}"),
                username: None,
            })
            .collect::<Vec<_>>();
        let backend_slots = (0..count)
            .map(|index| AvailabilitySlot::new(index).expect("synthetic count fits bitmap"))
            .collect::<Vec<_>>();
        let mask = backend_slots
            .iter()
            .map(|slot| slot.bit())
            .fold(0, |mask, bit| mask | bit);
        Self {
            backend_slots: backend_slots.into_boxed_slice(),
            identities: identities.into_boxed_slice(),
            mask,
            epoch: 0,
        }
    }

    /// Build the compact current-process layout, deduplicating host/account pairs.
    pub(crate) fn from_servers(servers: &[Server]) -> Result<Self, AvailabilityLayoutError> {
        let mut identities = Vec::new();

        for server in servers {
            let identity = AvailabilityIdentity::from_server(server);
            if !identities.contains(&identity) {
                identities.push(identity);
            }
        }
        let mut backend_slots = Vec::with_capacity(servers.len());
        for server in servers {
            let identity = AvailabilityIdentity::from_server(server);
            let slot = identities
                .iter()
                .position(|existing| existing == &identity)
                .expect("identity was collected above");
            backend_slots.push(
                AvailabilitySlot::new(slot).ok_or(AvailabilityLayoutError::TooManyIdentities)?,
            );
        }

        let mask = identities
            .iter()
            .enumerate()
            .map(|(index, _)| AvailabilitySlot::new(index).expect("validated slot").bit())
            .fold(0, |mask, bit| mask | bit);

        Ok(Self {
            backend_slots: backend_slots.into_boxed_slice(),
            identities: identities.into_boxed_slice(),
            mask,
            epoch: 0,
        })
    }

    /// Load or atomically publish the hybrid registry before cache startup.
    pub(crate) fn from_hybrid_registry(servers: &[Server], cache_path: &Path) -> Result<Self> {
        fs::create_dir_all(cache_path)
            .with_context(|| format!("create hybrid cache directory {}", cache_path.display()))?;
        let path = cache_path.join("availability.registry");
        let (mut identities, mut epoch) = match fs::read(&path) {
            Ok(data) => parse_registry(&data).unwrap_or_else(|_| (Vec::new(), 0)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), 0),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let registry_was_valid = epoch != 0;
        let mut changed = !registry_was_valid;
        for server in servers {
            let identity = AvailabilityIdentity::from_server(server);
            if !identities.contains(&identity) {
                identities.push(identity);
                changed = true;
            }
        }
        if identities.len() > usize::BITS as usize {
            anyhow::bail!("hybrid availability registry has no free slot");
        }
        if epoch == 0 {
            epoch = uuid::Uuid::new_v4().as_u128() as u64;
            if epoch == 0 {
                epoch = 1;
            }
            changed = true;
        }
        if changed {
            publish_registry(&path, epoch, &identities)?;
        }

        let mut backend_slots = Vec::with_capacity(servers.len());
        let mut configured_mask = 0;
        for server in servers {
            let identity = AvailabilityIdentity::from_server(server);
            let index = identities
                .iter()
                .position(|candidate| candidate == &identity)
                .expect("configured identity is in registry");
            let slot =
                AvailabilitySlot::new(index).ok_or(AvailabilityLayoutError::TooManyIdentities)?;
            configured_mask |= slot.bit();
            backend_slots.push(slot);
        }
        Ok(Self {
            backend_slots: backend_slots.into_boxed_slice(),
            identities: identities.into_boxed_slice(),
            mask: configured_mask,
            epoch,
        })
    }

    #[must_use]
    pub(crate) fn slot_for_backend(&self, backend: BackendId) -> AvailabilitySlot {
        self.backend_slots[backend.as_index()]
    }

    #[must_use]
    pub(crate) const fn identity_count(&self) -> usize {
        self.identities.len()
    }

    #[must_use]
    pub(crate) fn slot_for_identity(
        &self,
        identity: &AvailabilityIdentity,
    ) -> Option<AvailabilitySlot> {
        self.identities
            .iter()
            .position(|candidate| candidate == identity)
            .and_then(AvailabilitySlot::new)
    }

    #[must_use]
    pub(crate) fn identities(&self) -> &[AvailabilityIdentity] {
        &self.identities
    }

    #[must_use]
    pub(crate) fn fingerprint(&self) -> u64 {
        let mut hasher = XxHash64::default();
        let mut identities = self.identities.to_vec();
        identities.sort_unstable();
        for identity in &identities {
            hasher.write(identity.host.as_bytes());
            hasher.write_u8(0);
            if let Some(username) = &identity.username {
                hasher.write_u8(1);
                hasher.write(username.as_bytes());
            } else {
                hasher.write_u8(0);
            }
            hasher.write_u8(0xff);
        }
        hasher.finish().max(1)
    }

    #[must_use]
    pub(crate) fn availability_epoch(&self) -> u64 {
        if self.epoch == 0 {
            self.fingerprint()
        } else {
            self.epoch
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AvailabilityLayoutError {
    TooManyIdentities,
}

impl fmt::Display for AvailabilityLayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyIdentities => write!(f, "too many availability identities"),
        }
    }
}

impl std::error::Error for AvailabilityLayoutError {}

impl Ord for AvailabilityIdentity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.host
            .cmp(&other.host)
            .then_with(|| self.username.cmp(&other.username))
    }
}

impl PartialOrd for AvailabilityIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_registry(data: &[u8]) -> Result<(Vec<AvailabilityIdentity>, u64)> {
    if data.len() < REGISTRY_MAGIC.len() + 2 * size_of::<u64>()
        || &data[..REGISTRY_MAGIC.len()] != REGISTRY_MAGIC
    {
        anyhow::bail!("invalid hybrid availability registry");
    }
    let mut cursor = REGISTRY_MAGIC.len();
    let epoch = read_u64(data, &mut cursor)?;
    if epoch == 0 {
        anyhow::bail!("invalid hybrid availability registry epoch");
    }
    let count = usize::try_from(read_u64(data, &mut cursor)?)?;
    if count > usize::BITS as usize {
        anyhow::bail!("hybrid availability registry has too many identities");
    }
    let mut identities = Vec::with_capacity(count);
    for _ in 0..count {
        let host = read_string(data, &mut cursor, "hostname")?;
        let marker = *data
            .get(cursor)
            .ok_or_else(|| anyhow::anyhow!("truncated hybrid account marker"))?;
        cursor += 1;
        let username = match marker {
            0 => None,
            1 => Some(read_string(data, &mut cursor, "username")?),
            _ => anyhow::bail!("invalid hybrid account marker"),
        };
        let identity = AvailabilityIdentity { host, username };
        if identities.contains(&identity) {
            anyhow::bail!("duplicate hybrid availability identity");
        }
        identities.push(identity);
    }
    if cursor != data.len() {
        anyhow::bail!("trailing hybrid availability registry bytes");
    }
    Ok((identities, epoch))
}

fn publish_registry(path: &Path, epoch: u64, identities: &[AvailabilityIdentity]) -> Result<()> {
    let mut data = Vec::new();
    data.extend_from_slice(REGISTRY_MAGIC);
    data.extend_from_slice(&epoch.to_le_bytes());
    data.extend_from_slice(&(identities.len() as u64).to_le_bytes());
    for identity in identities {
        write_string(&mut data, &identity.host)?;
        match &identity.username {
            Some(username) => {
                data.push(1);
                write_string(&mut data, username)?;
            }
            None => data.push(0),
        }
    }
    let temporary = path.with_extension("registry.tmp");
    fs::write(&temporary, data).with_context(|| format!("write {}", temporary.display()))?;
    if let Err(error) = crate::io_util::atomic_replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("publish {}", path.display()));
    }
    Ok(())
}

fn write_string(data: &mut Vec<u8>, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() > MAX_REGISTRY_FIELD_BYTES {
        anyhow::bail!("hybrid availability identity field is too long");
    }
    data.extend_from_slice(&(u32::try_from(bytes.len())?).to_le_bytes());
    data.extend_from_slice(bytes);
    Ok(())
}

fn read_string(data: &[u8], cursor: &mut usize, field: &str) -> Result<String> {
    let length = usize::try_from(read_u32(data, cursor)?)?;
    if length == 0 || length > MAX_REGISTRY_FIELD_BYTES {
        anyhow::bail!("invalid hybrid availability {field} length");
    }
    let end = cursor
        .checked_add(length)
        .context("identity length overflow")?;
    let bytes = data
        .get(*cursor..end)
        .ok_or_else(|| anyhow::anyhow!("truncated hybrid availability {field}"))?;
    *cursor = end;
    Ok(String::from_utf8(bytes.to_vec())?)
}

fn read_u32(data: &[u8], cursor: &mut usize) -> Result<u32> {
    let bytes = data
        .get(*cursor..cursor.saturating_add(size_of::<u32>()))
        .ok_or_else(|| anyhow::anyhow!("truncated hybrid availability u32"))?;
    *cursor += size_of::<u32>();
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

fn read_u64(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let bytes = data
        .get(*cursor..cursor.saturating_add(size_of::<u64>()))
        .ok_or_else(|| anyhow::anyhow!("truncated hybrid availability u64"))?;
    *cursor += size_of::<u64>();
    Ok(u64::from_le_bytes(bytes.try_into()?))
}
