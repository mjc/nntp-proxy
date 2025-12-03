//! Cache capacity configuration types

use nutype::nutype;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Cache capacity in bytes
///
/// Supports human-readable formats:
/// - "1gb" = 1 GB
/// - "500mb" = 500 MB  
/// - "64mb" = 64 MB (default)
/// - 10000 = 10,000 bytes
#[nutype(
    validate(greater = 0),
    derive(
        Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, TryFrom, Into, AsRef,
    )
)]
pub struct CacheCapacity(u64);

impl Default for CacheCapacity {
    fn default() -> Self {
        // 64 MB - safe because we know it's > 0
        Self::try_new(64 * 1024 * 1024).expect("default capacity is valid")
    }
}

impl CacheCapacity {
    /// Get capacity in bytes
    #[inline]
    pub fn get(&self) -> u64 {
        self.into_inner()
    }

    /// Get capacity as u64 for moka
    #[inline]
    pub fn as_u64(&self) -> u64 {
        self.into_inner()
    }
}

impl std::str::FromStr for CacheCapacity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_lowercase();

        // Try to parse as bytesize (supports kb, mb, gb, etc)
        if let Ok(size) = s.parse::<bytesize::ByteSize>() {
            let bytes = size.as_u64();
            return Self::try_new(bytes).map_err(|e| e.to_string());
        }

        // Fallback to plain number (bytes)
        match s.parse::<u64>() {
            Ok(bytes) => Self::try_new(bytes).map_err(|e| e.to_string()),
            Err(_) => Err(format!("Invalid cache capacity: {}", s)),
        }
    }
}

impl std::fmt::Display for CacheCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", bytesize::ByteSize::b(self.into_inner()))
    }
}

impl Serialize for CacheCapacity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.into_inner().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CacheCapacity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StringOrU64 {
            String(String),
            U64(u64),
        }

        match StringOrU64::deserialize(deserializer)? {
            StringOrU64::U64(bytes) => Self::try_new(bytes).map_err(serde::de::Error::custom),
            StringOrU64::String(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        // 64 MB
        assert_eq!(CacheCapacity::default().get(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_zero_rejected() {
        assert!(CacheCapacity::try_new(0).is_err());
    }

    #[test]
    fn test_valid_values() {
        assert_eq!(CacheCapacity::try_new(1).unwrap().get(), 1);
        assert_eq!(CacheCapacity::try_new(5000).unwrap().get(), 5000);
    }

    #[test]
    fn test_from_str_bytes() {
        assert_eq!("2000".parse::<CacheCapacity>().unwrap().get(), 2000);
        assert!("0".parse::<CacheCapacity>().is_err());
        assert!("not_a_number".parse::<CacheCapacity>().is_err());
    }

    #[test]
    fn test_from_str_units() {
        // bytesize crate uses decimal (1 GB = 1,000,000,000 bytes)
        // not binary (1 GiB = 1,073,741,824 bytes)
        assert_eq!("1gb".parse::<CacheCapacity>().unwrap().get(), 1_000_000_000);
        assert_eq!("500mb".parse::<CacheCapacity>().unwrap().get(), 500_000_000);
        assert_eq!("10kb".parse::<CacheCapacity>().unwrap().get(), 10_000);

        // For binary units, use GiB/MiB/KiB
        assert_eq!(
            "1gib".parse::<CacheCapacity>().unwrap().get(),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn test_ordering() {
        let small = CacheCapacity::try_new(100).unwrap();
        let large = CacheCapacity::try_new(1000).unwrap();
        assert!(small < large);
    }
}
