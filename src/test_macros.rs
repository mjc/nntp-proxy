//! Test macros for NonZero newtype wrappers
//!
//! This module provides reusable test macros to reduce boilerplate
//! when testing newtype wrappers around NonZero* types.

/// Generate standard tests for a NonZero newtype wrapper
///
/// # Arguments
/// * `$type_name` - The newtype struct name (e.g., `CacheCapacity`)
/// * `$default_value` - Expected value of the DEFAULT constant
/// * `$test_value` - A valid non-zero value for testing
///
/// # Generated Tests
/// - `test_default` - Verifies DEFAULT constant value
/// - `test_new_valid` - Tests creating with valid non-zero value
/// - `test_new_zero_rejected` - Verifies zero returns None
/// - `test_clone_equality` - Tests Clone and PartialEq
///
/// # Example
/// ```ignore
/// test_nonzero_newtype!(CacheCapacity, 1000, 5000);
/// ```
#[macro_export]
macro_rules! test_nonzero_newtype {
    ($type_name:ident, $default_value:expr, $test_value:expr) => {
        #[test]
        fn test_default() {
            assert_eq!($type_name::DEFAULT.get(), $default_value);
        }

        #[test]
        fn test_new_valid() {
            let value = $type_name::new($test_value).unwrap();
            assert_eq!(value.get(), $test_value);
        }

        #[test]
        fn test_new_zero_rejected() {
            assert!($type_name::new(0).is_none());
        }

        #[test]
        fn test_clone_equality() {
            let val1 = $type_name::new($test_value).unwrap();
            let val2 = val1.clone();
            let val3 = $type_name::new($test_value).unwrap();
            let val4 = $type_name::new($default_value).unwrap();

            assert_eq!(val1, val2);
            assert_eq!(val1, val3);
            assert_ne!(val1, val4);
        }
    };
}

/// Generate Ord/PartialOrd tests for a newtype with ordering
///
/// # Arguments
/// * `$type_name` - The newtype struct name
/// * `$small_value` - A smaller value for comparison
/// * `$large_value` - A larger value for comparison
///
/// # Generated Tests
/// - `test_ordering` - Tests Ord, PartialOrd, and comparison operators
///
/// # Example
/// ```ignore
/// test_newtype_ordering!(CacheCapacity, 100, 1000);
/// ```
#[macro_export]
macro_rules! test_newtype_ordering {
    ($type_name:ident, $small_value:expr, $large_value:expr) => {
        #[test]
        fn test_ordering() {
            let small = $type_name::new($small_value).unwrap();
            let large = $type_name::new($large_value).unwrap();
            let equal = $type_name::new($small_value).unwrap();

            // Test comparison operators
            assert!(small < large);
            assert!(large > small);
            assert!(small <= large);
            assert!(large >= small);
            assert!(small <= equal);
            assert!(small >= equal);

            // Test PartialOrd
            assert_eq!(small.partial_cmp(&large), Some(std::cmp::Ordering::Less));
            assert_eq!(large.partial_cmp(&small), Some(std::cmp::Ordering::Greater));
            assert_eq!(small.partial_cmp(&equal), Some(std::cmp::Ordering::Equal));

            // Test Ord
            assert_eq!(small.cmp(&large), std::cmp::Ordering::Less);
            assert_eq!(large.cmp(&small), std::cmp::Ordering::Greater);
            assert_eq!(small.cmp(&equal), std::cmp::Ordering::Equal);
        }
    };
}

/// Generate FromStr tests for a newtype with FromStr implementation
///
/// # Arguments
/// * `$type_name` - The newtype struct name
/// * `$valid_str` - A valid string to parse
/// * `$expected_value` - Expected value after parsing
/// * `$invalid_str` - An invalid string that should fail to parse
///
/// # Generated Tests
/// - `test_from_str_valid` - Tests parsing valid string
/// - `test_from_str_invalid` - Tests parsing invalid string returns error
///
/// # Example
/// ```ignore
/// test_newtype_from_str!(CacheCapacity, "2000", 2000, "not_a_number");
/// ```
#[macro_export]
macro_rules! test_newtype_from_str {
    ($type_name:ident, $valid_str:expr, $expected_value:expr, $invalid_str:expr) => {
        #[test]
        fn test_from_str_valid() {
            let value: $type_name = $valid_str.parse().unwrap();
            assert_eq!(value.get(), $expected_value);
        }

        #[test]
        fn test_from_str_invalid() {
            let result: Result<$type_name, _> = $invalid_str.parse();
            assert!(result.is_err());
        }
    };
}

/// Generate all standard tests for a NonZero newtype with ordering and FromStr
///
/// This is a convenience macro that combines:
/// - test_nonzero_newtype!
/// - test_newtype_ordering!
/// - test_newtype_from_str!
///
/// # Example
/// ```ignore
/// test_nonzero_newtype_full!(
///     CacheCapacity,
///     default: 1000,
///     test_value: 5000,
///     ordering: (100, 1000),
///     from_str: ("2000", 2000, "invalid")
/// );
/// ```
#[macro_export]
macro_rules! test_nonzero_newtype_full {
    (
        $type_name:ident,
        default: $default_value:expr,
        test_value: $test_value:expr,
        ordering: ($small:expr, $large:expr),
        from_str: ($valid_str:expr, $expected:expr, $invalid_str:expr)
    ) => {
        $crate::test_nonzero_newtype!($type_name, $default_value, $test_value);
        $crate::test_newtype_ordering!($type_name, $small, $large);
        $crate::test_newtype_from_str!($type_name, $valid_str, $expected, $invalid_str);
    };
}
