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
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn valid_ports_roundtrip(port in 1u16..=65535u16) {
            let p = Port::new(port).unwrap();
            prop_assert_eq!(p.get(), port);
        }

        #[test]
        fn port_from_str_roundtrip(port in 1u16..=65535u16) {
            let port_str = port.to_string();
            let parsed: Port = port_str.parse().unwrap();
            prop_assert_eq!(parsed.get(), port);
        }

        #[test]
        fn port_try_from_roundtrip(port in 1u16..=65535u16) {
            let p = Port::try_from(port).unwrap();
            prop_assert_eq!(p.get(), port);
        }

        #[test]
        fn port_equality_same_value(port in 1u16..=65535u16) {
            let p1 = Port::new(port).unwrap();
            let p2 = Port::new(port).unwrap();
            prop_assert_eq!(p1, p2);
        }

        #[test]
        fn port_clone_equality(port in 1u16..=65535u16) {
            let p1 = Port::new(port).unwrap();
            let p2 = p1;
            prop_assert_eq!(p1, p2);
        }

        #[test]
        fn port_ordering(a in 1u16..=65535u16, b in 1u16..=65535u16) {
            let port_a = Port::new(a).unwrap();
            let port_b = Port::new(b).unwrap();
            prop_assert_eq!(port_a.cmp(&port_b), a.cmp(&b));
            prop_assert_eq!(port_a.partial_cmp(&port_b), Some(a.cmp(&b)));
        }

        #[test]
        fn port_display_shows_value(port in 1u16..=65535u16) {
            let p = Port::new(port).unwrap();
            let display = format!("{}", p);
            prop_assert!(display.contains(&port.to_string()));
        }
    }

    // Edge case tests
    #[test]
    fn port_zero_is_invalid() {
        assert!(Port::new(0).is_none());
    }

    #[test]
    fn port_try_from_zero_fails() {
        let result = Port::try_from(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ValidationError::InvalidPort));
    }

    #[test]
    fn port_from_str_zero_fails() {
        let result: Result<Port, _> = "0".parse();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ValidationError::InvalidPort));
    }

    #[test]
    fn port_from_str_invalid_text() {
        let result: Result<Port, _> = "not_a_port".parse();
        assert!(result.is_err());
    }

    #[test]
    fn port_from_str_out_of_range() {
        let result: Result<Port, _> = "65536".parse();
        assert!(result.is_err());
    }

    #[test]
    fn port_from_str_negative() {
        let result: Result<Port, _> = "-1".parse();
        assert!(result.is_err());
    }

    // Constant verification
    #[test]
    fn port_constants_correct() {
        assert_eq!(Port::NNTP.get(), 119);
        assert_eq!(Port::NNTPS.get(), 563);
        assert_eq!(Port::DEFAULT.get(), 8119);
    }

    #[test]
    fn port_default_matches_constant() {
        assert_eq!(Port::default(), Port::DEFAULT);
        assert_eq!(Port::default().get(), 8119);
    }

    #[test]
    fn port_max_value_is_valid() {
        let port = Port::new(65535).unwrap();
        assert_eq!(port.get(), 65535);
    }
}
