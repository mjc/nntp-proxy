//! Cache capacity configuration types

use nutype::nutype;

/// A non-zero cache capacity
///
/// Ensures caches track at least 1 item (default: 1000)
#[nutype(
    validate(greater = 0),
    default = 1000,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        FromStr,
        Serialize,
        Deserialize,
        Default
    )
)]
pub struct CacheCapacity(usize);

impl CacheCapacity {
    /// Get the inner value (convenience method)
    #[inline]
    pub fn get(&self) -> usize {
        self.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        assert_eq!(CacheCapacity::default().get(), 1000);
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
    fn test_from_str() {
        assert_eq!("2000".parse::<CacheCapacity>().unwrap().get(), 2000);
        assert!("0".parse::<CacheCapacity>().is_err());
        assert!("not_a_number".parse::<CacheCapacity>().is_err());
    }

    #[test]
    fn test_ordering() {
        let small = CacheCapacity::try_new(100).unwrap();
        let large = CacheCapacity::try_new(1000).unwrap();
        assert!(small < large);
    }
}
