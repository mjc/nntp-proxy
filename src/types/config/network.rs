//! Network-related configuration types

use nutype::nutype;

/// A validated network port number that cannot be zero
#[nutype(
    validate(greater = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        TryFrom,
        AsRef,
        Deref,
        Serialize,
        Deserialize
    )
)]
#[doc(alias = "port_number")]
#[doc(alias = "tcp_port")]
pub struct Port(u16);

impl Port {
    /// NNTP port (119)
    pub const NNTP: u16 = 119;

    /// NNTPS port (563)
    pub const NNTPS: u16 = 563;

    /// Default proxy listen port (8119)
    pub const DEFAULT: u16 = 8119;

    /// Get the inner value
    #[inline]
    pub fn get(&self) -> u16 {
        self.into_inner()
    }

    /// Create NNTP port instance
    pub fn nntp() -> Self {
        Self::try_new(Self::NNTP).unwrap()
    }

    /// Create NNTPS port instance
    pub fn nntps() -> Self {
        Self::try_new(Self::NNTPS).unwrap()
    }
}

impl Default for Port {
    fn default() -> Self {
        Self::try_new(Self::DEFAULT).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_invalid() {
        assert!(Port::try_new(0).is_err());
    }

    #[test]
    fn constants_are_valid() {
        assert_eq!(Port::NNTP, 119);
        assert_eq!(Port::NNTPS, 563);
        assert_eq!(Port::DEFAULT, 8119);
    }

    #[test]
    fn helper_methods_work() {
        assert_eq!(Port::nntp().get(), 119);
        assert_eq!(Port::nntps().get(), 563);
    }
}
