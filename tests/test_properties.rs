//! Property-based tests using proptest
//!
//! These tests verify invariants and algebraic properties of core functions
//! using property-based testing with arbitrary input generation.

use nntp_proxy::cache::ArticleAvailability;
use nntp_proxy::cache::ttl::effective_ttl;
use nntp_proxy::command::NntpCommand;
use nntp_proxy::protocol::StatusCode;
use nntp_proxy::types::MessageId;
use proptest::prelude::*;

// =============================================================================
// 1. NntpCommand::parse - Classifier robustness and consistency
// =============================================================================

proptest! {
    #[test]
    fn prop_parse_never_panics(s in ".*") {
        // Parse any string without panicking
        let _ = NntpCommand::parse(&s);
    }

    #[test]
    fn prop_case_insensitive_known_commands(
        cmd in r"(ARTICLE|BODY|HEAD|STAT|GROUP|QUIT|LIST|DATE|HELP|NEXT|LAST)",
        arg in r"[a-z0-9@.<>-]*"
    ) {
        let upper = format!("{} {}", cmd, arg);
        let lower = format!("{} {}", cmd.to_lowercase(), arg);

        let upper_result = NntpCommand::parse(&upper);
        let lower_result = NntpCommand::parse(&lower);

        // Same classification regardless of case
        prop_assert_eq!(
            std::mem::discriminant(&upper_result),
            std::mem::discriminant(&lower_result),
            "UPPER vs lower case differ: {} vs {}",
            upper,
            lower
        );
    }

    #[test]
    fn prop_trimming_idempotent(s in ".*") {
        let with_spaces = format!("  {}  ", s);
        let result1 = NntpCommand::parse(&with_spaces);
        let result2 = NntpCommand::parse(&s);

        // Trimming is idempotent
        prop_assert_eq!(
            std::mem::discriminant(&result1),
            std::mem::discriminant(&result2),
            "Trimming changed classification: '{}' vs '{}'",
            with_spaces,
            s
        );
    }

    #[test]
    fn prop_article_by_msgid_requires_brackets(
        cmd in r"(ARTICLE|BODY|HEAD|STAT)",
        arg in r"[a-z0-9@.+_-]+"
    ) {
        // With angle brackets, should be ArticleByMessageId
        let with_brackets = format!("{} <{}>", cmd, arg);
        match NntpCommand::parse(&with_brackets) {
            NntpCommand::ArticleByMessageId => {
                // Expected
            }
            other => {
                prop_assert!(false, "Should be ArticleByMessageId with brackets: {}, got {:?}", with_brackets, other);
            }
        }
    }

    #[test]
    fn prop_parse_deterministic(s in ".*") {
        // Same input always produces same classification
        let result1 = NntpCommand::parse(&s);
        let result2 = NntpCommand::parse(&s);

        prop_assert_eq!(
            std::mem::discriminant(&result1),
            std::mem::discriminant(&result2),
            "Parse not deterministic for: {}",
            s
        );
    }
}

// =============================================================================
// 2. ArticleAvailability - Bitset algebraic properties
// =============================================================================

proptest! {
    #[test]
    fn prop_availability_never_panics(
        ops in prop::collection::vec((0..8u8, 0u8..2), 0..50)
    ) {
        let mut avail = ArticleAvailability::new();
        for (backend_id, op) in ops {
            let id = nntp_proxy::types::BackendId::from_index(backend_id as usize);
            match op {
                0 => { avail.record_missing(id); }
                _ => { avail.record_has(id); }
            }
        }
    }

    #[test]
    fn prop_record_missing_idempotent(backend_id in 0..8u8) {
        use nntp_proxy::types::BackendId;
        let id = BackendId::from_index(backend_id as usize);
        let mut avail1 = ArticleAvailability::new();
        let mut avail2 = ArticleAvailability::new();

        avail1.record_missing(id);
        avail1.record_missing(id);  // Second call

        avail2.record_missing(id);  // Only once

        prop_assert_eq!(avail1.is_missing(id), avail2.is_missing(id));
    }

    #[test]
    fn prop_record_has_clears_missing(backend_id in 0..8u8) {
        use nntp_proxy::types::BackendId;
        let id = BackendId::from_index(backend_id as usize);
        let mut avail = ArticleAvailability::new();

        avail.record_missing(id);
        prop_assert!(avail.is_missing(id));

        avail.record_has(id);
        prop_assert!(!avail.is_missing(id));
    }

    #[test]
    fn prop_should_try_inverse_of_is_missing(backend_id in 0..8u8) {
        use nntp_proxy::types::BackendId;
        let id = BackendId::from_index(backend_id as usize);
        let mut avail = ArticleAvailability::new();

        // Initially should_try is true, is_missing is false
        prop_assert_eq!(avail.should_try(id), !avail.is_missing(id));

        // After record_missing, inverted
        avail.record_missing(id);
        prop_assert_eq!(avail.should_try(id), !avail.is_missing(id));

        // After record_has, back to original
        avail.record_has(id);
        prop_assert_eq!(avail.should_try(id), !avail.is_missing(id));
    }

    #[test]
    fn prop_all_exhausted_consistency(
        num_backends in 1..=8usize
    ) {
        use nntp_proxy::types::BackendId;
        use nntp_proxy::router::BackendCount;
        let mut avail = ArticleAvailability::new();
        let count = BackendCount::new(num_backends);

        // Initially not exhausted
        prop_assert!(!avail.all_exhausted(count));

        // Mark all as missing
        for i in 0..num_backends {
            avail.record_missing(BackendId::from_index(i));
        }

        // Now exhausted
        prop_assert!(avail.all_exhausted(count));
    }
}

// =============================================================================
// 3. StatusCode::parse - Response parsing robustness
// =============================================================================

proptest! {
    #[test]
    fn prop_statuscode_parse_never_panics(data in prop::collection::vec(any::<u8>(), 0..100)) {
        // Parse any bytes without panicking
        let _ = StatusCode::parse(&data);
    }

    #[test]
    fn prop_statuscode_roundtrip(code in 0u16..=999u16) {
        let formatted = format!("{:03}", code).into_bytes();
        if let Some(parsed) = StatusCode::parse(&formatted) {
            prop_assert_eq!(parsed.as_u16(), code);
        }
    }

    #[test]
    fn prop_classification_mutually_exclusive(code in 100u16..=599u16) {
        let formatted = format!("{:03}", code).into_bytes();
        if let Some(status) = StatusCode::parse(&formatted) {
            let is_success = status.is_success();
            let is_error = status.is_error();
            let is_info = status.is_informational();

            // Success and error are mutually exclusive
            prop_assert!(!(is_success && is_error),
                "Code {} cannot be both success and error", code);

            // Informational and success/error are mutually exclusive
            if is_info {
                prop_assert!(!is_success && !is_error,
                    "Code {} is informational but also success/error", code);
            }
        }
    }

    #[test]
    fn prop_success_codes_2xx_to_3xx(code in 200u16..=399u16) {
        let formatted = format!("{:03}", code).into_bytes();
        if let Some(status) = StatusCode::parse(&formatted) {
            prop_assert!(status.is_success(), "Code {} should be success", code);
        }
    }

    #[test]
    fn prop_error_codes_4xx_to_5xx(code in 400u16..=599u16) {
        let formatted = format!("{:03}", code).into_bytes();
        if let Some(status) = StatusCode::parse(&formatted) {
            prop_assert!(status.is_error(), "Code {} should be error", code);
        }
    }
}

// =============================================================================
// 4. MessageId - Validation and roundtrip properties
// =============================================================================

proptest! {
    #[test]
    fn prop_messageid_requires_brackets(s in r"[a-zA-Z0-9@.+_-]+") {
        let with_brackets = format!("<{}>", s);
        let without = s.clone();

        // With brackets should succeed
        let result_with = MessageId::new(with_brackets.clone());
        prop_assert!(result_with.is_ok(), "Valid format should succeed: {}", &with_brackets);

        // Without brackets should fail
        let result_without = MessageId::new(without.clone());
        prop_assert!(result_without.is_err(), "Without brackets should fail: {}", &without);
    }

    #[test]
    fn prop_messageid_without_brackets_inverse(s in r"[a-zA-Z0-9@.+_-]+") {
        let with_brackets = format!("<{}>", s);
        if let Ok(msg_id) = MessageId::new(with_brackets.clone()) {
            // Reconstruction should give original
            let reconstructed = format!("<{}>", msg_id.without_brackets());
            prop_assert_eq!(&reconstructed, &with_brackets,
                "Reconstruction failed for: {}", &with_brackets);
        }
    }

    #[test]
    fn prop_messageid_as_str_consistency(s in r"<[a-zA-Z0-9@.+_-]+>") {
        let s_str = s.clone();
        if let Ok(msg_id) = MessageId::new(s) {
            // as_str() should return the original string
            prop_assert_eq!(msg_id.as_str(), s_str,
                "as_str() should match original");
        }
    }

    #[test]
    fn prop_messageid_from_str_or_wrap_idempotent(s in ".*") {
        // First wrap
        if let Ok(wrapped1) = MessageId::from_str_or_wrap(&s) {
            // Double wrap should be same as single
            if let Ok(wrapped2) = MessageId::from_str_or_wrap(wrapped1.as_str()) {
                prop_assert_eq!(wrapped1.as_str(), wrapped2.as_str(),
                    "from_str_or_wrap not idempotent for: {}", &s);
            }
        }
    }
}

// =============================================================================
// 5. effective_ttl - Arithmetic monotonicity
// =============================================================================

proptest! {
    #[test]
    fn prop_effective_ttl_never_panics(
        base in any::<u64>(),
        tier in any::<u8>()
    ) {
        // Should work for all combinations
        let _ = effective_ttl(base, tier);
    }

    #[test]
    fn prop_effective_ttl_monotonic_in_base(
        b1 in 0u64..1000u64,
        b2 in 0u64..1000u64,
        tier in 0u8..=10u8
    ) {
        if b1 <= b2 {
            let ttl1 = effective_ttl(b1, tier);
            let ttl2 = effective_ttl(b2, tier);

            // Higher base should give higher or equal TTL
            prop_assert!(ttl1 <= ttl2,
                "TTL not monotonic in base: base {} -> {}, tier {}, got {} and {}",
                b1, b2, tier, ttl1, ttl2);
        }
    }

    #[test]
    fn prop_effective_ttl_monotonic_in_tier(
        base in 1u64..1000u64,
        t1 in 0u8..20u8,
        t2 in 0u8..20u8
    ) {
        if t1 <= t2 {
            let ttl1 = effective_ttl(base, t1);
            let ttl2 = effective_ttl(base, t2);

            // Higher tier should give higher or equal TTL
            prop_assert!(ttl1 <= ttl2,
                "TTL not monotonic in tier: base {}, tier {} -> {}, got {} and {}",
                base, t1, t2, ttl1, ttl2);
        }
    }

    #[test]
    fn prop_effective_ttl_zero_base_is_zero(tier in any::<u8>()) {
        let ttl = effective_ttl(0, tier);
        prop_assert_eq!(ttl, 0, "Zero base should always give zero TTL");
    }
}
