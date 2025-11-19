//! Network-related configuration types

use std::num::NonZeroU16;
use std::str::FromStr;

use crate::types::ValidationError;

nonzero_newtype! {
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
    pub struct Port(NonZeroU16: u16, serialize as serialize_u16);
}

impl Port {
    /// NNTP port (119)
    pub const NNTP: Self = Self(NonZeroU16::new(119).unwrap());

    /// NNTPS port (563)
    pub const NNTPS: Self = Self(NonZeroU16::new(563).unwrap());

    /// Default proxy listen port (8119)
    pub const DEFAULT: Self = Self(NonZeroU16::new(8119).unwrap());
}

impl Default for Port {
    /// Default to port 8119 (common NNTP proxy port)
    fn default() -> Self {
        Self::DEFAULT
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

impl PartialOrd for Port {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Port {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.get().cmp(&other.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_default() {
        let port = Port::default();
        assert_eq!(port.get(), 8119);
        assert_eq!(port, Port::DEFAULT);
    }

    #[test]
    fn test_port_constants() {
        assert_eq!(Port::NNTP.get(), 119);
        assert_eq!(Port::NNTPS.get(), 563);
        assert_eq!(Port::DEFAULT.get(), 8119);
    }

    #[test]
    fn test_port_new_valid() {
        let port = Port::new(8080).unwrap();
        assert_eq!(port.get(), 8080);
    }

    #[test]
    fn test_port_new_zero_returns_none() {
        assert!(Port::new(0).is_none());
    }

    #[test]
    fn test_port_new_max_value() {
        let port = Port::new(65535).unwrap();
        assert_eq!(port.get(), 65535);
    }

    #[test]
    fn test_port_from_str_valid() {
        let port: Port = "443".parse().unwrap();
        assert_eq!(port.get(), 443);
    }

    #[test]
    fn test_port_from_str_zero() {
        let result: Result<Port, _> = "0".parse();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ValidationError::InvalidPort));
    }

    #[test]
    fn test_port_from_str_invalid() {
        let result: Result<Port, _> = "not_a_port".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_port_from_str_out_of_range() {
        let result: Result<Port, _> = "65536".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_port_from_str_negative() {
        let result: Result<Port, _> = "-1".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_port_try_from_valid() {
        let port = Port::try_from(80).unwrap();
        assert_eq!(port.get(), 80);
    }

    #[test]
    fn test_port_try_from_zero() {
        let result = Port::try_from(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ValidationError::InvalidPort));
    }

    #[test]
    fn test_port_try_from_max() {
        let port = Port::try_from(65535).unwrap();
        assert_eq!(port.get(), 65535);
    }

    #[test]
    fn test_port_partial_ord() {
        let small = Port::new(80).unwrap();
        let large = Port::new(8080).unwrap();

        assert!(small < large);
        assert!(large > small);
        assert_eq!(small.partial_cmp(&large), Some(std::cmp::Ordering::Less));
    }

    #[test]
    fn test_port_ord() {
        let small = Port::new(80).unwrap();
        let large = Port::new(8080).unwrap();
        let equal = Port::new(80).unwrap();

        assert_eq!(small.cmp(&large), std::cmp::Ordering::Less);
        assert_eq!(large.cmp(&small), std::cmp::Ordering::Greater);
        assert_eq!(small.cmp(&equal), std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_port_equality() {
        let port1 = Port::new(443).unwrap();
        let port2 = Port::new(443).unwrap();
        let port3 = Port::new(8443).unwrap();

        assert_eq!(port1, port2);
        assert_ne!(port1, port3);
    }

    #[test]
    fn test_port_standard_ports() {
        // Common HTTP/HTTPS ports
        assert!(Port::new(80).is_some());
        assert!(Port::new(443).is_some());

        // Common NNTP ports
        assert_eq!(Port::NNTP.get(), 119);
        assert_eq!(Port::NNTPS.get(), 563);
    }
}
