//! Duration serialization helpers for configuration files

use serde::{Deserialize, Deserializer, Serializer};
use std::time::Duration;

/// Helper for deserializing Duration from seconds
///
/// TOML/JSON configs typically specify durations in seconds, so we need
/// custom serde to convert from u64 seconds to Duration
pub mod duration_serde {
    use super::*;

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

/// Helper for deserializing `Option<Duration>` from seconds
pub mod option_duration_serde {
    use super::*;

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
    use proptest::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestDuration {
        #[serde(with = "duration_serde")]
        timeout: Duration,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestOptionDuration {
        #[serde(with = "option_duration_serde")]
        timeout: Option<Duration>,
    }

    proptest! {
        /// Property: Duration serialization round-trips correctly for any valid second value
        #[test]
        fn prop_duration_serde_roundtrip(secs in 0u64..100000) {
            let original = TestDuration {
                timeout: Duration::from_secs(secs),
            };
            let json = serde_json::to_string(&original).unwrap();
            let parsed: TestDuration = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, parsed);
        }

        /// Property: Duration JSON format is always `{"timeout":N}`
        #[test]
        fn prop_duration_json_format(secs in 0u64..100000) {
            let test = TestDuration {
                timeout: Duration::from_secs(secs),
            };
            let json = serde_json::to_string(&test).unwrap();
            let expected = format!(r#"{{"timeout":{}}}"#, secs);
            prop_assert_eq!(json, expected);
        }

        /// Property: Option<Duration> with Some serializes to number
        #[test]
        fn prop_option_duration_some_roundtrip(secs in 0u64..100000) {
            let original = TestOptionDuration {
                timeout: Some(Duration::from_secs(secs)),
            };
            let json = serde_json::to_string(&original).unwrap();
            let parsed: TestOptionDuration = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, parsed);
        }

        /// Property: TOML serialization round-trips correctly
        #[test]
        fn prop_duration_toml_roundtrip(secs in 0u64..100000) {
            let original = TestDuration {
                timeout: Duration::from_secs(secs),
            };
            let toml = toml::to_string(&original).unwrap();
            let parsed: TestDuration = toml::from_str(&toml).unwrap();
            prop_assert_eq!(original, parsed);
        }
    }

    // Edge cases
    #[test]
    fn test_option_duration_none_roundtrip() {
        let original = TestOptionDuration { timeout: None };
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, r#"{"timeout":null}"#);

        let parsed: TestOptionDuration = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_option_duration_deserialize_null() {
        let json = r#"{"timeout":null}"#;
        let test: TestOptionDuration = serde_json::from_str(json).unwrap();
        assert_eq!(test.timeout, None);
    }

    #[test]
    fn test_option_duration_deserialize_missing_field() {
        let json = r#"{}"#;
        let result: Result<TestOptionDuration, _> = serde_json::from_str(json);
        // serde will error on missing required field
        assert!(result.is_err());
    }
}
