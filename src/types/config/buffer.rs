//! Buffer and window size configuration types

use nutype::nutype;

/// A non-zero window size for health check tracking
#[nutype(
    validate(greater = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        Display,
        TryFrom,
        AsRef,
        Deref,
        Serialize,
        Deserialize
    )
)]
pub struct WindowSize(u64);

impl WindowSize {
    /// Default window size
    pub const DEFAULT: u64 = 100;

    /// Get the inner value
    #[inline]
    pub fn get(&self) -> u64 {
        self.into_inner()
    }
}

/// A non-zero buffer size
#[nutype(
    validate(greater = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        Display,
        TryFrom,
        AsRef,
        Deref,
        Serialize,
        Deserialize
    )
)]
pub struct BufferSize(usize);

impl BufferSize {
    /// Standard NNTP command buffer (512 bytes)
    pub const COMMAND: usize = 512;

    /// Default buffer size (8KB)
    pub const DEFAULT: usize = 8192;

    /// Medium buffer size (256KB)
    pub const MEDIUM: usize = 256 * 1024;

    /// Large buffer size (4MB)
    pub const LARGE: usize = 4 * 1024 * 1024;

    /// Get the inner value
    #[inline]
    pub fn get(&self) -> usize {
        self.into_inner()
    }

    /// Create command buffer size instance
    pub fn command() -> Self {
        Self::try_new(Self::COMMAND).unwrap()
    }

    /// Create medium buffer size instance
    pub fn medium() -> Self {
        Self::try_new(Self::MEDIUM).unwrap()
    }

    /// Create large buffer size instance
    pub fn large() -> Self {
        Self::try_new(Self::LARGE).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Property-based tests for BufferSize
    proptest! {
        #[test]
        fn valid_buffer_sizes_roundtrip(size in 1usize..10_000_000usize) {
            let buffer = BufferSize::try_new(size).unwrap();
            prop_assert_eq!(buffer.get(), size);
        }

        #[test]
        fn buffer_size_display_shows_value(size in 1usize..10_000_000usize) {
            let buffer = BufferSize::try_new(size).unwrap();
            let display = format!("{}", buffer);
            prop_assert!(display.contains(&size.to_string()));
        }

        #[test]
        fn buffer_size_debug_shows_value(size in 1usize..10_000_000usize) {
            let buffer = BufferSize::try_new(size).unwrap();
            let debug = format!("{:?}", buffer);
            prop_assert!(debug.contains("BufferSize"));
            prop_assert!(debug.contains(&size.to_string()));
        }

        #[test]
        fn buffer_size_serde_json_roundtrip(size in 1usize..10_000_000usize) {
            let original = BufferSize::try_new(size).unwrap();
            let json = serde_json::to_string(&original).unwrap();
            let deserialized: BufferSize = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, deserialized);
        }

        #[test]
        fn buffer_size_clone_equality(size in 1usize..10_000_000usize) {
            let original = BufferSize::try_new(size).unwrap();
            let cloned = original;
            prop_assert_eq!(original, cloned);
            prop_assert_eq!(original.get(), cloned.get());
        }

        #[test]
        fn buffer_size_equality_same_value(size in 1usize..10_000_000usize) {
            let buffer1 = BufferSize::try_new(size).unwrap();
            let buffer2 = BufferSize::try_new(size).unwrap();
            prop_assert_eq!(buffer1, buffer2);
        }
    }

    // Property-based tests for WindowSize
    proptest! {
        #[test]
        fn valid_window_sizes_roundtrip(size in 1u64..100_000u64) {
            let window = WindowSize::try_new(size).unwrap();
            prop_assert_eq!(window.get(), size);
        }

        #[test]
        fn window_size_display_shows_value(size in 1u64..100_000u64) {
            let window = WindowSize::try_new(size).unwrap();
            let display = format!("{}", window);
            prop_assert!(display.contains(&size.to_string()));
        }

        #[test]
        fn window_size_serde_json_roundtrip(size in 1u64..100_000u64) {
            let original = WindowSize::try_new(size).unwrap();
            let json = serde_json::to_string(&original).unwrap();
            let deserialized: WindowSize = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, deserialized);
        }

        #[test]
        fn window_size_clone_equality(size in 1u64..100_000u64) {
            let original = WindowSize::try_new(size).unwrap();
            let cloned = original;
            prop_assert_eq!(original, cloned);
        }
    }

    // Edge case tests (keep explicit tests for boundaries)
    #[test]
    fn buffer_size_zero_is_invalid() {
        assert!(BufferSize::try_new(0).is_err());
    }

    #[test]
    fn buffer_size_one_is_valid() {
        let size = BufferSize::try_new(1).unwrap();
        assert_eq!(size.get(), 1);
    }

    #[test]
    fn window_size_zero_is_invalid() {
        assert!(WindowSize::try_new(0).is_err());
    }

    // Constant verification tests
    #[test]
    fn buffer_size_constants_correct_values() {
        assert_eq!(BufferSize::COMMAND, 512);
        assert_eq!(BufferSize::DEFAULT, 8192);
        assert_eq!(BufferSize::MEDIUM, 262_144);
        assert_eq!(BufferSize::LARGE, 4_194_304);
    }

    // Ordering test removed - constants are compile-time known

    #[test]
    fn window_size_default_constant() {
        assert_eq!(WindowSize::DEFAULT, 100);
    }
}
