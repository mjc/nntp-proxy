//! Network-related configuration types

use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU16;
use std::str::FromStr;

use crate::types::ValidationError;

/// A validated network port number that cannot be zero
///
/// This type ensures at compile time that port numbers are always valid (1-65535).
/// Port 0 is reserved and cannot be used for actual network communication.
///
/// # Examples
/// ```
/// use nntp_proxy::types::Port;
///
/// let port = Port::new(119).unwrap();
/// assert_eq!(port.get(), 119);
///
/// // Port 0 is invalid
/// assert!(Port::new(0).is_none());
///
/// // Standard NNTP port
/// let nntp = Port::NNTP;
/// assert_eq!(nntp.get(), 119);
/// ```
#[doc(alias = "port_number")]
#[doc(alias = "tcp_port")]
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Port(NonZeroU16);

impl Port {
    /// Create a new Port from a u16, returning None if port is 0
    #[must_use]
    pub const fn new(port: u16) -> Option<Self> {
        match NonZeroU16::new(port) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the port number as u16
    #[must_use]
    #[inline]
    pub const fn get(&self) -> u16 {
        self.0.get()
    }

    /// NNTP port (119)
    /// Safety: 119 is a non-zero, valid u16 value
    pub const NNTP: Self = Self(NonZeroU16::new(119).unwrap());

    /// NNTPS port (563)
    /// Safety: 563 is a non-zero, valid u16 value
    pub const NNTPS: Self = Self(NonZeroU16::new(563).unwrap());
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl FromStr for Port {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let port = s
            .parse::<u16>()
            .map_err(|_| ValidationError::InvalidHostName(format!("invalid port number: {}", s)))?;
        Self::new(port).ok_or(ValidationError::InvalidPort)
    }
}

impl TryFrom<u16> for Port {
    type Error = ValidationError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(ValidationError::InvalidPort)
    }
}

impl From<Port> for u16 {
    fn from(port: Port) -> Self {
        port.get()
    }
}

impl Serialize for Port {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u16(self.get())
    }
}

impl<'de> Deserialize<'de> for Port {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let port = u16::deserialize(deserializer)?;
        Self::new(port).ok_or_else(|| serde::de::Error::custom("port cannot be 0"))
    }
}
