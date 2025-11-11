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
