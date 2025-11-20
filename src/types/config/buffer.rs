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

    // WindowSize tests
    #[test]
    fn test_window_size_default() {
        let size = WindowSize::DEFAULT;
        assert_eq!(size.get(), 100);
    }

    #[test]
    fn test_window_size_new_valid() {
        let size = WindowSize::new(50).unwrap();
        assert_eq!(size.get(), 50);
    }

    #[test]
    fn test_window_size_new_zero_returns_none() {
        assert!(WindowSize::new(0).is_none());
    }

    #[test]
    fn test_window_size_new_large() {
        let size = WindowSize::new(10000).unwrap();
        assert_eq!(size.get(), 10000);
    }

    #[test]
    fn test_window_size_display() {
        let size = WindowSize::new(100).unwrap();
        assert_eq!(format!("{}", size), "100");
    }

    #[test]
    fn test_window_size_clone() {
        let size1 = WindowSize::new(50).unwrap();
        let size2 = size1.clone();
        assert_eq!(size1.get(), size2.get());
    }

    #[test]
    fn test_window_size_equality() {
        let size1 = WindowSize::new(100).unwrap();
        let size2 = WindowSize::new(100).unwrap();
        let size3 = WindowSize::new(200).unwrap();

        assert_eq!(size1, size2);
        assert_ne!(size1, size3);
    }

    // BufferSize tests
    #[test]
    fn test_buffer_size_command_constant() {
        assert_eq!(BufferSize::COMMAND.get(), 512);
    }

    #[test]
    fn test_buffer_size_default_constant() {
        assert_eq!(BufferSize::DEFAULT.get(), 8192);
    }

    #[test]
    fn test_buffer_size_medium_constant() {
        assert_eq!(BufferSize::MEDIUM.get(), 256 * 1024);
    }

    #[test]
    fn test_buffer_size_large_constant() {
        assert_eq!(BufferSize::LARGE.get(), 4 * 1024 * 1024);
    }

    #[test]
    fn test_buffer_size_new_valid() {
        let size = BufferSize::new(1024).unwrap();
        assert_eq!(size.get(), 1024);
    }

    #[test]
    fn test_buffer_size_new_zero_returns_none() {
        assert!(BufferSize::new(0).is_none());
    }

    #[test]
    fn test_buffer_size_new_one() {
        let size = BufferSize::new(1).unwrap();
        assert_eq!(size.get(), 1);
    }

    #[test]
    fn test_buffer_size_display() {
        let size = BufferSize::new(8192).unwrap();
        assert_eq!(format!("{}", size), "8192");
    }

    #[test]
    fn test_buffer_size_clone() {
        let size1 = BufferSize::COMMAND;
        let size2 = size1.clone();
        assert_eq!(size1.get(), size2.get());
    }

    #[test]
    fn test_buffer_size_equality() {
        let size1 = BufferSize::new(1024).unwrap();
        let size2 = BufferSize::new(1024).unwrap();
        let size3 = BufferSize::new(2048).unwrap();

        assert_eq!(size1, size2);
        assert_ne!(size1, size3);
    }

    #[test]
    fn test_buffer_size_constants_are_powers_of_two_or_multiples() {
        // COMMAND is 512 = 2^9
        assert_eq!(BufferSize::COMMAND.get(), 512);

        // DEFAULT is 8KB = 2^13
        assert_eq!(BufferSize::DEFAULT.get(), 8192);

        // MEDIUM is 256KB = 2^18
        assert_eq!(BufferSize::MEDIUM.get(), 262_144);

        // LARGE is 4MB = 2^22
        assert_eq!(BufferSize::LARGE.get(), 4_194_304);
    }

    #[test]
    fn test_buffer_size_ordering() {
        assert!(BufferSize::COMMAND.get() < BufferSize::DEFAULT.get());
        assert!(BufferSize::DEFAULT.get() < BufferSize::MEDIUM.get());
        assert!(BufferSize::MEDIUM.get() < BufferSize::LARGE.get());
    }

    #[test]
    fn test_window_size_serde_json() {
        let size = WindowSize::new(150).unwrap();
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "150");

        let deserialized: WindowSize = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.get(), 150);
    }

    #[test]
    fn test_buffer_size_serde_json() {
        let size = BufferSize::new(4096).unwrap();
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "4096");

        let deserialized: BufferSize = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.get(), 4096);
    }
}
