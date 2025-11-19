//! Cache capacity configuration types

use std::num::NonZeroUsize;

nonzero_newtype! {
    /// A non-zero cache capacity
    ///
    /// Ensures caches track at least 1 item
    pub struct CacheCapacity(NonZeroUsize: usize, serialize as serialize_u64);
}

impl CacheCapacity {
    /// Default cache capacity (1000 items)
    pub const DEFAULT: Self = Self(NonZeroUsize::new(1000).unwrap());
}

impl std::str::FromStr for CacheCapacity {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.parse::<usize>()?;
        Ok(Self::new(value).unwrap_or(Self::DEFAULT))
    }
}

impl PartialOrd for CacheCapacity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CacheCapacity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.get().cmp(&other.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_capacity() {
        let capacity = CacheCapacity::DEFAULT;
        assert_eq!(capacity.get(), 1000);
    }

    #[test]
    fn test_new_valid_capacity() {
        let capacity = CacheCapacity::new(5000).unwrap();
        assert_eq!(capacity.get(), 5000);
    }

    #[test]
    fn test_new_zero_capacity_returns_none() {
        let capacity = CacheCapacity::new(0);
        assert!(capacity.is_none());
    }

    #[test]
    fn test_from_str_valid() {
        let capacity: CacheCapacity = "2000".parse().unwrap();
        assert_eq!(capacity.get(), 2000);
    }

    #[test]
    fn test_from_str_zero_defaults() {
        // Zero values should default to DEFAULT (1000)
        let capacity: CacheCapacity = "0".parse().unwrap();
        assert_eq!(capacity.get(), 1000);
    }

    #[test]
    fn test_from_str_invalid() {
        let result: Result<CacheCapacity, _> = "not_a_number".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_partial_ord() {
        let small = CacheCapacity::new(100).unwrap();
        let large = CacheCapacity::new(1000).unwrap();

        assert!(small < large);
        assert!(large > small);
        assert_eq!(small.partial_cmp(&large), Some(std::cmp::Ordering::Less));
    }

    #[test]
    fn test_ord() {
        let small = CacheCapacity::new(100).unwrap();
        let large = CacheCapacity::new(1000).unwrap();
        let equal = CacheCapacity::new(100).unwrap();

        assert_eq!(small.cmp(&large), std::cmp::Ordering::Less);
        assert_eq!(large.cmp(&small), std::cmp::Ordering::Greater);
        assert_eq!(small.cmp(&equal), std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_equality() {
        let cap1 = CacheCapacity::new(500).unwrap();
        let cap2 = CacheCapacity::new(500).unwrap();
        let cap3 = CacheCapacity::new(1000).unwrap();

        assert_eq!(cap1, cap2);
        assert_ne!(cap1, cap3);
    }
}
