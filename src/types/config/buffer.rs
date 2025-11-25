//! Buffer and window size configuration types

use std::num::{NonZeroU64, NonZeroUsize};

nonzero_newtype! {
    /// A non-zero window size for health check tracking
    ///
    /// Ensures health check windows track at least 1 request
    pub struct WindowSize(NonZeroU64: u64, serialize as serialize_u64);
}

impl WindowSize {
    /// Default window size (typically 100)
    pub const DEFAULT: Self = Self(NonZeroU64::new(100).unwrap());
}

nonzero_newtype! {
    /// A non-zero buffer size
    ///
    /// Ensures buffers always have at least 1 byte
    pub struct BufferSize(NonZeroUsize: usize, serialize as serialize_u64);
}

impl BufferSize {
    /// Standard NNTP command buffer (512 bytes)
    pub const COMMAND: Self = Self(NonZeroUsize::new(512).unwrap());

    /// Default buffer size (8KB)
    pub const DEFAULT: Self = Self(NonZeroUsize::new(8192).unwrap());

    /// Medium buffer size (256KB)
    pub const MEDIUM: Self = Self(NonZeroUsize::new(256 * 1024).unwrap());

    /// Large buffer size (4MB)
    pub const LARGE: Self = Self(NonZeroUsize::new(4 * 1024 * 1024).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Property-based tests for BufferSize
    proptest! {
        #[test]
        fn valid_buffer_sizes_roundtrip(size in 1usize..10_000_000usize) {
            let buffer = BufferSize::new(size).unwrap();
            prop_assert_eq!(buffer.get(), size);
        }

        #[test]
        fn buffer_size_display_shows_value(size in 1usize..10_000_000usize) {
            let buffer = BufferSize::new(size).unwrap();
            let display = format!("{}", buffer);
            prop_assert!(display.contains(&size.to_string()));
        }

        #[test]
        fn buffer_size_debug_shows_value(size in 1usize..10_000_000usize) {
            let buffer = BufferSize::new(size).unwrap();
            let debug = format!("{:?}", buffer);
            prop_assert!(debug.contains("BufferSize"));
            prop_assert!(debug.contains(&size.to_string()));
        }

        #[test]
        fn buffer_size_serde_json_roundtrip(size in 1usize..10_000_000usize) {
            let original = BufferSize::new(size).unwrap();
            let json = serde_json::to_string(&original).unwrap();
            let deserialized: BufferSize = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, deserialized);
        }

        #[test]
        fn buffer_size_clone_equality(size in 1usize..10_000_000usize) {
            let original = BufferSize::new(size).unwrap();
            let cloned = original;
            prop_assert_eq!(original, cloned);
            prop_assert_eq!(original.get(), cloned.get());
        }

        #[test]
        fn buffer_size_equality_same_value(size in 1usize..10_000_000usize) {
            let buffer1 = BufferSize::new(size).unwrap();
            let buffer2 = BufferSize::new(size).unwrap();
            prop_assert_eq!(buffer1, buffer2);
        }
    }

    // Property-based tests for WindowSize
    proptest! {
        #[test]
        fn valid_window_sizes_roundtrip(size in 1u64..100_000u64) {
            let window = WindowSize::new(size).unwrap();
            prop_assert_eq!(window.get(), size);
        }

        #[test]
        fn window_size_display_shows_value(size in 1u64..100_000u64) {
            let window = WindowSize::new(size).unwrap();
            let display = format!("{}", window);
            prop_assert!(display.contains(&size.to_string()));
        }

        #[test]
        fn window_size_serde_json_roundtrip(size in 1u64..100_000u64) {
            let original = WindowSize::new(size).unwrap();
            let json = serde_json::to_string(&original).unwrap();
            let deserialized: WindowSize = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, deserialized);
        }

        #[test]
        fn window_size_clone_equality(size in 1u64..100_000u64) {
            let original = WindowSize::new(size).unwrap();
            let cloned = original;
            prop_assert_eq!(original, cloned);
        }
    }

    // Edge case tests (keep explicit tests for boundaries)
    #[test]
    fn buffer_size_zero_is_invalid() {
        assert!(BufferSize::new(0).is_none());
    }

    #[test]
    fn buffer_size_one_is_valid() {
        let size = BufferSize::new(1).unwrap();
        assert_eq!(size.get(), 1);
    }

    #[test]
    fn window_size_zero_is_invalid() {
        assert!(WindowSize::new(0).is_none());
    }

    // Constant verification tests
    #[test]
    fn buffer_size_constants_correct_values() {
        assert_eq!(BufferSize::COMMAND.get(), 512);
        assert_eq!(BufferSize::DEFAULT.get(), 8192);
        assert_eq!(BufferSize::MEDIUM.get(), 262_144);
        assert_eq!(BufferSize::LARGE.get(), 4_194_304);
    }

    #[test]
    fn buffer_size_constants_ordering() {
        assert!(BufferSize::COMMAND.get() < BufferSize::DEFAULT.get());
        assert!(BufferSize::DEFAULT.get() < BufferSize::MEDIUM.get());
        assert!(BufferSize::MEDIUM.get() < BufferSize::LARGE.get());
    }

    #[test]
    fn window_size_default_constant() {
        assert_eq!(WindowSize::DEFAULT.get(), 100);
    }
}
