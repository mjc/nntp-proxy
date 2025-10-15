//! Configuration-related type-safe wrappers using NonZero types

use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64, NonZeroUsize};

use super::ValidationError;

/// A validated network port number that cannot be zero
///
/// This type ensures at compile time that port numbers are always valid (1-65535).
/// Port 0 is reserved and cannot be used for actual network communication.
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

/// A non-zero maximum connections limit
///
/// Ensures connection pools always have at least 1 connection allowed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxConnections(NonZeroUsize);

impl MaxConnections {
    /// Create a new MaxConnections, returning None if value is 0
    #[must_use]
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as usize
    #[must_use]
    #[inline]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    /// Default maximum connections per backend
    pub const DEFAULT: Self = Self(NonZeroUsize::new(10).unwrap());
}

impl fmt::Display for MaxConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<MaxConnections> for usize {
    fn from(max: MaxConnections) -> Self {
        max.get()
    }
}

impl Serialize for MaxConnections {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as u64)
    }
}

impl<'de> Deserialize<'de> for MaxConnections {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("max_connections cannot be 0"))
    }
}

/// A non-zero cache capacity
///
/// Ensures caches always have at least 1 slot available
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheCapacity(NonZeroUsize);

impl CacheCapacity {
    /// Create a new CacheCapacity, returning None if value is 0
    #[must_use]
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as usize
    #[must_use]
    #[inline]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    /// Default pool size
    pub const DEFAULT: Self = Self(NonZeroUsize::new(1000).unwrap());
}

impl fmt::Display for CacheCapacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<CacheCapacity> for usize {
    fn from(cap: CacheCapacity) -> Self {
        cap.get()
    }
}

impl Serialize for CacheCapacity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as u64)
    }
}

impl<'de> Deserialize<'de> for CacheCapacity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("cache capacity cannot be 0"))
    }
}

/// A non-zero maximum errors threshold
///
/// Ensures health check thresholds are meaningful (at least 1 error required)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxErrors(NonZeroU32);

impl MaxErrors {
    /// Create a new MaxErrors, returning None if value is 0
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as u32
    #[must_use]
    #[inline]
    pub const fn get(&self) -> u32 {
        self.0.get()
    }

    /// Default retry attempts
    pub const DEFAULT: Self = Self(NonZeroU32::new(3).unwrap());
}

impl fmt::Display for MaxErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<MaxErrors> for u32 {
    fn from(max: MaxErrors) -> Self {
        max.get()
    }
}

impl Serialize for MaxErrors {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.get())
    }
}

impl<'de> Deserialize<'de> for MaxErrors {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("max_errors cannot be 0"))
    }
}

/// A non-zero window size for health check tracking
///
/// Ensures health check windows track at least 1 request
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowSize(NonZeroU64);

impl WindowSize {
    /// Create a new WindowSize, returning None if value is 0
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as u64
    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0.get()
    }

    /// Default window size (typically 100)
    pub const DEFAULT: Self = Self(NonZeroU64::new(100).unwrap());
}

impl fmt::Display for WindowSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<WindowSize> for u64 {
    fn from(size: WindowSize) -> Self {
        size.get()
    }
}

impl Serialize for WindowSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for WindowSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("window_size cannot be 0"))
    }
}

/// A non-zero buffer size
///
/// Ensures buffers always have at least 1 byte
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferSize(NonZeroUsize);

impl BufferSize {
    /// Create a new BufferSize, returning None if value is 0
    #[must_use]
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as usize
    #[must_use]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    /// Standard NNTP command buffer (512 bytes)
    pub const COMMAND: Self = Self(NonZeroUsize::new(512).unwrap());

    /// Default buffer size (8KB)
    pub const DEFAULT: Self = Self(NonZeroUsize::new(8192).unwrap());

    /// Medium buffer size (256KB)
    pub const MEDIUM: Self = Self(NonZeroUsize::new(256 * 1024).unwrap());

    /// Large buffer size (4MB)
    pub const LARGE: Self = Self(NonZeroUsize::new(4 * 1024 * 1024).unwrap());
}

impl fmt::Display for BufferSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<BufferSize> for usize {
    fn from(size: BufferSize) -> Self {
        size.get()
    }
}

impl Serialize for BufferSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as u64)
    }
}

impl<'de> Deserialize<'de> for BufferSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("buffer size cannot be 0"))
    }
}

/// Helper for deserializing Duration from seconds
///
/// TOML/JSON configs typically specify durations in seconds, so we need
/// custom serde to convert from u64 seconds to Duration
pub mod duration_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Helper for deserializing Option<Duration> from seconds
pub mod option_duration_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(d) => serializer.serialize_some(&d.as_secs()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = Option::<u64>::deserialize(deserializer)?;
        Ok(secs.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Port tests
    #[test]
    fn test_port_valid() {
        let port = Port::new(8080).unwrap();
        assert_eq!(port.get(), 8080);
    }

    #[test]
    fn test_port_zero_rejected() {
        assert!(Port::new(0).is_none());
    }

    #[test]
    fn test_port_max() {
        let port = Port::new(65535).unwrap();
        assert_eq!(port.get(), 65535);
    }

    #[test]
    fn test_port_constants() {
        assert_eq!(Port::NNTP.get(), 119);
        assert_eq!(Port::NNTPS.get(), 563);
    }

    #[test]
    fn test_port_display() {
        let port = Port::new(8080).unwrap();
        assert_eq!(format!("{}", port), "8080");
    }

    #[test]
    fn test_port_try_from() {
        let port: Port = 8080u16.try_into().unwrap();
        assert_eq!(port.get(), 8080);

        let result: Result<Port, _> = 0u16.try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_port_into_u16() {
        let port = Port::new(8080).unwrap();
        let value: u16 = port.into();
        assert_eq!(value, 8080);
    }

    #[test]
    fn test_port_serde() {
        let port = Port::new(8080).unwrap();
        let json = serde_json::to_string(&port).unwrap();
        assert_eq!(json, "8080");

        let deserialized: Port = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, port);
    }

    #[test]
    fn test_port_serde_zero_rejected() {
        let json = "0";
        let result: Result<Port, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_port_ordering() {
        let port1 = Port::new(80).unwrap();
        let port2 = Port::new(443).unwrap();
        assert!(port1 < port2);
    }

    #[test]
    fn test_port_const() {
        const NNTP: Port = Port::NNTP;
        assert_eq!(NNTP.get(), 119);
    }

    // MaxConnections tests
    #[test]
    fn test_max_connections_valid() {
        let max = MaxConnections::new(10).unwrap();
        assert_eq!(max.get(), 10);
    }

    #[test]
    fn test_max_connections_zero_rejected() {
        assert!(MaxConnections::new(0).is_none());
    }

    #[test]
    fn test_max_connections_default() {
        assert_eq!(MaxConnections::DEFAULT.get(), 10);
    }

    #[test]
    fn test_max_connections_display() {
        let max = MaxConnections::new(20).unwrap();
        assert_eq!(format!("{}", max), "20");
    }

    #[test]
    fn test_max_connections_into_usize() {
        let max = MaxConnections::new(30).unwrap();
        let value: usize = max.into();
        assert_eq!(value, 30);
    }

    #[test]
    fn test_max_connections_serde() {
        let max = MaxConnections::new(15).unwrap();
        let json = serde_json::to_string(&max).unwrap();
        assert_eq!(json, "15");

        let deserialized: MaxConnections = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, max);
    }

    #[test]
    fn test_max_connections_serde_zero_rejected() {
        let json = "0";
        let result: Result<MaxConnections, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // CacheCapacity tests
    #[test]
    fn test_cache_capacity_valid() {
        let cap = CacheCapacity::new(1000).unwrap();
        assert_eq!(cap.get(), 1000);
    }

    #[test]
    fn test_cache_capacity_zero_rejected() {
        assert!(CacheCapacity::new(0).is_none());
    }

    #[test]
    fn test_cache_capacity_default() {
        assert_eq!(CacheCapacity::DEFAULT.get(), 1000);
    }

    #[test]
    fn test_cache_capacity_display() {
        let cap = CacheCapacity::new(500).unwrap();
        assert_eq!(format!("{}", cap), "500");
    }

    #[test]
    fn test_cache_capacity_into_usize() {
        let cap = CacheCapacity::new(2000).unwrap();
        let value: usize = cap.into();
        assert_eq!(value, 2000);
    }

    #[test]
    fn test_cache_capacity_serde() {
        let cap = CacheCapacity::new(750).unwrap();
        let json = serde_json::to_string(&cap).unwrap();
        assert_eq!(json, "750");

        let deserialized: CacheCapacity = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, cap);
    }

    #[test]
    fn test_cache_capacity_serde_zero_rejected() {
        let json = "0";
        let result: Result<CacheCapacity, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // MaxErrors tests
    #[test]
    fn test_max_errors_valid() {
        let max = MaxErrors::new(5).unwrap();
        assert_eq!(max.get(), 5);
    }

    #[test]
    fn test_max_errors_zero_rejected() {
        assert!(MaxErrors::new(0).is_none());
    }

    #[test]
    fn test_max_errors_default() {
        assert_eq!(MaxErrors::DEFAULT.get(), 3);
    }

    #[test]
    fn test_max_errors_display() {
        let max = MaxErrors::new(7).unwrap();
        assert_eq!(format!("{}", max), "7");
    }

    #[test]
    fn test_max_errors_into_u32() {
        let max = MaxErrors::new(10).unwrap();
        let value: u32 = max.into();
        assert_eq!(value, 10);
    }

    #[test]
    fn test_max_errors_serde() {
        let max = MaxErrors::new(8).unwrap();
        let json = serde_json::to_string(&max).unwrap();
        assert_eq!(json, "8");

        let deserialized: MaxErrors = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, max);
    }

    #[test]
    fn test_max_errors_serde_zero_rejected() {
        let json = "0";
        let result: Result<MaxErrors, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // WindowSize tests
    #[test]
    fn test_window_size_valid() {
        let size = WindowSize::new(100).unwrap();
        assert_eq!(size.get(), 100);
    }

    #[test]
    fn test_window_size_zero_rejected() {
        assert!(WindowSize::new(0).is_none());
    }

    #[test]
    fn test_window_size_default() {
        assert_eq!(WindowSize::DEFAULT.get(), 100);
    }

    #[test]
    fn test_window_size_display() {
        let size = WindowSize::new(200).unwrap();
        assert_eq!(format!("{}", size), "200");
    }

    #[test]
    fn test_window_size_into_u64() {
        let size = WindowSize::new(150).unwrap();
        let value: u64 = size.into();
        assert_eq!(value, 150);
    }

    #[test]
    fn test_window_size_serde() {
        let size = WindowSize::new(300).unwrap();
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "300");

        let deserialized: WindowSize = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, size);
    }

    #[test]
    fn test_window_size_serde_zero_rejected() {
        let json = "0";
        let result: Result<WindowSize, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // BufferSize tests
    #[test]
    fn test_buffer_size_valid() {
        let size = BufferSize::new(8192).unwrap();
        assert_eq!(size.get(), 8192);
    }

    #[test]
    fn test_buffer_size_zero_rejected() {
        assert!(BufferSize::new(0).is_none());
    }

    #[test]
    fn test_buffer_size_constants() {
        assert_eq!(BufferSize::COMMAND.get(), 512);
        assert_eq!(BufferSize::DEFAULT.get(), 8192);
        assert_eq!(BufferSize::MEDIUM.get(), 256 * 1024);
        assert_eq!(BufferSize::LARGE.get(), 4 * 1024 * 1024);
    }

    #[test]
    fn test_buffer_size_display() {
        let size = BufferSize::new(1024).unwrap();
        assert_eq!(format!("{}", size), "1024");
    }

    #[test]
    fn test_buffer_size_into_usize() {
        let size = BufferSize::new(2048).unwrap();
        let value: usize = size.into();
        assert_eq!(value, 2048);
    }

    #[test]
    fn test_buffer_size_serde() {
        let size = BufferSize::new(4096).unwrap();
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "4096");

        let deserialized: BufferSize = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, size);
    }

    #[test]
    fn test_buffer_size_serde_zero_rejected() {
        let json = "0";
        let result: Result<BufferSize, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // Duration serde tests
    #[test]
    fn test_duration_serde_roundtrip() {
        #[derive(Serialize, Deserialize)]
        struct Config {
            #[serde(with = "duration_serde")]
            timeout: Duration,
        }

        let config = Config {
            timeout: Duration::from_secs(30),
        };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("30"));

        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_option_duration_serde_some() {
        #[derive(Serialize, Deserialize)]
        struct Config {
            #[serde(with = "option_duration_serde")]
            timeout: Option<Duration>,
        }

        let config = Config {
            timeout: Some(Duration::from_secs(60)),
        };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("60"));

        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_option_duration_serde_none() {
        #[derive(Serialize, Deserialize)]
        struct Config {
            #[serde(with = "option_duration_serde")]
            timeout: Option<Duration>,
        }

        let config = Config { timeout: None };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("null"));

        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timeout, None);
    }

    #[test]
    fn test_duration_from_large_value() {
        #[derive(Serialize, Deserialize)]
        struct Config {
            #[serde(with = "duration_serde")]
            timeout: Duration,
        }

        let json = r#"{"timeout": 86400}"#; // 24 hours
        let deserialized: Config = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.timeout, Duration::from_secs(86400));
    }
}
