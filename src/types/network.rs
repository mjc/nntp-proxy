//! Network-related domain types with validation and type safety.

use derive_more::{AsRef, Deref, Display, From, Into};
use std::net::SocketAddr;

/// A validated client socket address.
///
/// This newtype wrapper provides type safety and prevents mixing up
/// client addresses with other socket addresses in the codebase.
///
/// # Examples
///
/// ```
/// use std::net::SocketAddr;
/// use nntp_proxy::types::ClientAddress;
///
/// let addr: SocketAddr = "127.0.0.1:8119".parse().unwrap();
/// let client_addr = ClientAddress::from(addr);
///
/// println!("Client connected from: {}", client_addr);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, From, Into, AsRef, Deref)]
pub struct ClientAddress(SocketAddr);

impl ClientAddress {
    /// Create a new client address from a socket address.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::SocketAddr;
    /// use nntp_proxy::types::ClientAddress;
    ///
    /// let addr: SocketAddr = "192.168.1.100:12345".parse().unwrap();
    /// let client_addr = ClientAddress::new(addr);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    /// Get the underlying socket address.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::SocketAddr;
    /// use nntp_proxy::types::ClientAddress;
    ///
    /// let addr: SocketAddr = "10.0.0.5:9999".parse().unwrap();
    /// let client_addr = ClientAddress::from(addr);
    ///
    /// assert_eq!(client_addr.as_socket_addr(), &addr);
    /// ```
    #[inline]
    #[must_use]
    pub const fn as_socket_addr(&self) -> &SocketAddr {
        &self.0
    }

    /// Convert into the underlying socket address.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::SocketAddr;
    /// use nntp_proxy::types::ClientAddress;
    ///
    /// let addr: SocketAddr = "172.16.0.1:5555".parse().unwrap();
    /// let client_addr = ClientAddress::from(addr);
    /// let socket_addr = client_addr.into_inner();
    ///
    /// assert_eq!(socket_addr, addr);
    /// ```
    #[inline]
    #[must_use]
    pub const fn into_inner(self) -> SocketAddr {
        self.0
    }

    /// Check if this is an IPv4 address.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::types::ClientAddress;
    /// use std::net::SocketAddr;
    ///
    /// let addr = ClientAddress::from("127.0.0.1:8119".parse::<SocketAddr>().unwrap());
    /// assert!(addr.is_ipv4());
    ///
    /// let addr6 = ClientAddress::from("[::1]:8119".parse::<SocketAddr>().unwrap());
    /// assert!(!addr6.is_ipv4());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_ipv4(&self) -> bool {
        self.0.is_ipv4()
    }

    /// Check if this is an IPv6 address.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::types::ClientAddress;
    /// use std::net::SocketAddr;
    ///
    /// let addr = ClientAddress::from("[::1]:8119".parse::<SocketAddr>().unwrap());
    /// assert!(addr.is_ipv6());
    ///
    /// let addr4 = ClientAddress::from("127.0.0.1:8119".parse::<SocketAddr>().unwrap());
    /// assert!(!addr4.is_ipv6());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_ipv6(&self) -> bool {
        self.0.is_ipv6()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_display() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)), 12345);
        let client_addr = ClientAddress::from(addr);
        assert_eq!(format!("{}", client_addr), "192.168.1.100:12345");
    }

    #[test]
    fn test_debug() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let client_addr = ClientAddress::from(addr);
        assert_eq!(
            format!("{:?}", client_addr),
            "ClientAddress(127.0.0.1:8119)"
        );
    }

    #[test]
    fn test_is_ipv4() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let client_addr = ClientAddress::from(addr);
        assert!(client_addr.is_ipv4());
        assert!(!client_addr.is_ipv6());
    }

    #[test]
    fn test_is_ipv6() {
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8119);
        let client_addr = ClientAddress::from(addr);
        assert!(client_addr.is_ipv6());
        assert!(!client_addr.is_ipv4());
    }

    #[test]
    fn test_equality() {
        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let addr3 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8120);

        let client1 = ClientAddress::from(addr1);
        let client2 = ClientAddress::from(addr2);
        let client3 = ClientAddress::from(addr3);

        assert_eq!(client1, client2);
        assert_ne!(client1, client3);
    }

    #[test]
    fn test_hash() {
        use std::collections::HashSet;

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);

        let client1 = ClientAddress::from(addr1);
        let client2 = ClientAddress::from(addr2);

        let mut set = HashSet::new();
        set.insert(client1);
        assert!(set.contains(&client2));
    }

    #[test]
    fn test_deref() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let client_addr = ClientAddress::from(addr);

        // Can call SocketAddr methods directly via Deref
        assert!(client_addr.is_ipv4());
        assert_eq!(client_addr.port(), 8119);
    }

    #[test]
    fn test_as_ref() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let client_addr = ClientAddress::from(addr);

        let socket_ref: &SocketAddr = client_addr.as_ref();
        assert_eq!(socket_ref, &addr);
    }
}
