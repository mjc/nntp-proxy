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

    // Use consolidated test macros for common patterns
    test_nonzero_newtype_full!(
        CacheCapacity,
        default: 1000,
        test_value: 5000,
        ordering: (100, 1000),
        from_str: ("2000", 2000, "not_a_number")
    );

    // Special test for zero defaulting to DEFAULT
    #[test]
    fn test_from_str_zero_defaults() {
        // Zero values should default to DEFAULT (1000)
        let capacity: CacheCapacity = "0".parse().unwrap();
        assert_eq!(capacity.get(), 1000);
    }
}
