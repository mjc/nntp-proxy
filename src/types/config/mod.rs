//! Configuration-related type-safe wrappers using NonZero types
//!
//! This module provides validated configuration types that enforce
//! invariants at the type level using Rust's NonZero types.

mod buffer;
mod cache;
pub mod duration;
mod limits;
mod network;

// Re-export all public types
pub use buffer::{BufferSize, WindowSize};
pub use cache::CacheCapacity;
pub use duration::{duration_serde, option_duration_serde};
pub use limits::{MaxConnections, MaxErrors, ThreadCount};
pub use network::Port;

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
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
