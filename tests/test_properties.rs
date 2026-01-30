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

// =============================================================================
// 6. Article::parse - Parser robustness
// =============================================================================

proptest! {
    #[test]
    fn prop_article_parse_never_panics(data in prop::collection::vec(any::<u8>(), 0..200)) {
        // Article::parse should never panic on arbitrary bytes
        let _ = nntp_proxy::protocol::Article::parse(&data, false);
        let _ = nntp_proxy::protocol::Article::parse(&data, true);
    }

    #[test]
    fn prop_article_220_has_headers_and_body(
        article_num in 0u64..100000u64,
        msg_local in r"[a-zA-Z0-9._+-]{1,20}",
        msg_domain in r"[a-zA-Z0-9.-]{1,20}",
        header_name in r"[A-Za-z][A-Za-z0-9-]{0,10}",
        header_value in r"[A-Za-z0-9 ]{1,30}",
        body_text in r"[A-Za-z0-9 ]{1,50}"
    ) {
        let msg_id = format!("<{}@{}>", msg_local, msg_domain);
        let buf = format!(
            "220 {} {} article\r\n{}: {}\r\n\r\n{}\r\n.\r\n",
            article_num, msg_id, header_name, header_value, body_text
        );
        let result = nntp_proxy::protocol::Article::parse(buf.as_bytes(), false);
        if let Ok(article) = result {
            prop_assert!(article.headers.is_some(),
                "220 response should have headers");
            prop_assert!(article.body.is_some(),
                "220 response should have body");
            prop_assert_eq!(article.message_id.as_str(), msg_id.as_str(),
                "Message ID should match");
        }
    }

    #[test]
    fn prop_article_221_has_headers_no_body(
        article_num in 0u64..100000u64,
        msg_local in r"[a-zA-Z0-9._+-]{1,20}",
        msg_domain in r"[a-zA-Z0-9.-]{1,20}",
        header_name in r"[A-Za-z][A-Za-z0-9-]{0,10}",
        header_value in r"[A-Za-z0-9 ]{1,30}"
    ) {
        let msg_id = format!("<{}@{}>", msg_local, msg_domain);
        let buf = format!(
            "221 {} {} head\r\n{}: {}\r\n.\r\n",
            article_num, msg_id, header_name, header_value
        );
        let result = nntp_proxy::protocol::Article::parse(buf.as_bytes(), false);
        if let Ok(article) = result {
            prop_assert!(article.headers.is_some(),
                "221 response should have headers");
            prop_assert!(article.body.is_none(),
                "221 response should NOT have body");
        }
    }

    #[test]
    fn prop_article_222_has_body_no_headers(
        article_num in 0u64..100000u64,
        msg_local in r"[a-zA-Z0-9._+-]{1,20}",
        msg_domain in r"[a-zA-Z0-9.-]{1,20}",
        body_text in r"[A-Za-z0-9 ]{1,50}"
    ) {
        let msg_id = format!("<{}@{}>", msg_local, msg_domain);
        let buf = format!(
            "222 {} {} body\r\n{}\r\n.\r\n",
            article_num, msg_id, body_text
        );
        let result = nntp_proxy::protocol::Article::parse(buf.as_bytes(), false);
        if let Ok(article) = result {
            prop_assert!(article.headers.is_none(),
                "222 response should NOT have headers");
            prop_assert!(article.body.is_some(),
                "222 response should have body");
        }
    }

    #[test]
    fn prop_article_223_has_neither(
        article_num in 0u64..100000u64,
        msg_local in r"[a-zA-Z0-9._+-]{1,20}",
        msg_domain in r"[a-zA-Z0-9.-]{1,20}"
    ) {
        let msg_id = format!("<{}@{}>", msg_local, msg_domain);
        let buf = format!(
            "223 {} {}\r\n.\r\n",
            article_num, msg_id
        );
        let result = nntp_proxy::protocol::Article::parse(buf.as_bytes(), false);
        if let Ok(article) = result {
            prop_assert!(article.headers.is_none(),
                "223 response should NOT have headers");
            prop_assert!(article.body.is_none(),
                "223 response should NOT have body");
        }
    }

    #[test]
    fn prop_article_message_id_format_validated(
        article_num in 0u64..100000u64,
        msg_local in r"[a-zA-Z0-9._+-]{1,20}",
        msg_domain in r"[a-zA-Z0-9.-]{1,20}"
    ) {
        let msg_id = format!("<{}@{}>", msg_local, msg_domain);
        let buf = format!(
            "223 {} {}\r\n.\r\n",
            article_num, msg_id
        );
        if let Ok(article) = nntp_proxy::protocol::Article::parse(buf.as_bytes(), false) {
            // Message ID must start with '<' and end with '>'
            let id_str = article.message_id.as_str();
            prop_assert!(id_str.starts_with('<'), "Message ID must start with '<': {}", id_str);
            prop_assert!(id_str.ends_with('>'), "Message ID must end with '>': {}", id_str);
        }
    }
}

// =============================================================================
// 7. NntpResponse::parse - Consistency properties
// =============================================================================

use nntp_proxy::protocol::NntpResponse;

proptest! {
    #[test]
    fn prop_nntp_response_parse_never_panics(data in prop::collection::vec(any::<u8>(), 0..100)) {
        // NntpResponse::parse should never panic on arbitrary bytes
        let _ = NntpResponse::parse(&data);
    }

    #[test]
    fn prop_nntp_response_invalid_iff_statuscode_fails(data in prop::collection::vec(any::<u8>(), 0..100)) {
        let response = NntpResponse::parse(&data);
        let status = StatusCode::parse(&data);

        // If StatusCode::parse fails, NntpResponse must be Invalid
        if status.is_none() {
            prop_assert_eq!(response, NntpResponse::Invalid,
                "NntpResponse should be Invalid when StatusCode::parse fails");
        }
        // If StatusCode::parse succeeds, NntpResponse must NOT be Invalid
        if status.is_some() {
            prop_assert_ne!(response, NntpResponse::Invalid,
                "NntpResponse should NOT be Invalid when StatusCode::parse succeeds");
        }
    }

    #[test]
    fn prop_nntp_response_non_invalid_has_status_code(code in 100u16..=599u16) {
        let formatted = format!("{:03} some text\r\n", code);
        let response = NntpResponse::parse(formatted.as_bytes());

        // Non-Invalid responses must have a status code
        if response != NntpResponse::Invalid {
            let sc = response.status_code();
            prop_assert!(sc.is_some(),
                "Non-Invalid response for code {} should have status_code()", code);
            prop_assert_eq!(sc.unwrap().as_u16(), code,
                "Status code should match input code {}", code);
        }
    }

    #[test]
    fn prop_nntp_response_multiline_consistency(code in 100u16..=599u16) {
        let formatted = format!("{:03} text\r\n", code);
        let response = NntpResponse::parse(formatted.as_bytes());

        if let Some(sc) = StatusCode::parse(formatted.as_bytes()) {
            // NntpResponse.is_multiline() should agree with StatusCode.is_multiline()
            // EXCEPT for special codes that NntpResponse categorizes differently
            // (200, 201 as Greeting; 205 as Disconnect; 281 as AuthSuccess; 381/480 as AuthRequired)
            let special = matches!(code, 200 | 201 | 205 | 281 | 381 | 480);
            if !special {
                prop_assert_eq!(response.is_multiline(), sc.is_multiline(),
                    "Multiline consistency for code {}: response={}, statuscode={}",
                    code, response.is_multiline(), sc.is_multiline());
            }
        }
    }

    #[test]
    fn prop_nntp_response_success_error_exclusive(code in 100u16..=599u16) {
        let formatted = format!("{:03} text\r\n", code);
        let response = NntpResponse::parse(formatted.as_bytes());

        if response != NntpResponse::Invalid {
            let is_success = response.is_success();
            let sc = response.status_code().unwrap();
            let is_error = sc.is_error();

            // Success and error must be mutually exclusive
            prop_assert!(!(is_success && is_error),
                "Code {} cannot be both success and error", code);
        }
    }
}

// =============================================================================
// 8. Headers::parse - RFC parser robustness
// =============================================================================

use nntp_proxy::protocol::Headers;

proptest! {
    #[test]
    fn prop_headers_parse_never_panics(data in prop::collection::vec(any::<u8>(), 0..200)) {
        // Headers::parse should never panic on arbitrary bytes
        let _ = Headers::parse(&data);
    }

    #[test]
    fn prop_headers_valid_roundtrip(
        name in r"[A-Za-z][A-Za-z0-9-]{0,15}",
        value in r"[A-Za-z0-9 ]{1,30}"
    ) {
        let raw = format!("{}: {}\r\n", name, value);
        let result = Headers::parse(raw.as_bytes());
        if let Ok(headers) = result {
            // as_bytes() should return the original data
            prop_assert_eq!(headers.as_bytes(), raw.as_bytes(),
                "Roundtrip failed for header: {}", raw);
        }
    }

    #[test]
    fn prop_headers_case_insensitive_lookup(
        name in r"[A-Za-z][A-Za-z0-9-]{0,15}",
        value in r"[A-Za-z0-9 ]{1,30}"
    ) {
        let raw = format!("{}: {}\r\n", name, value);
        if let Ok(headers) = Headers::parse(raw.as_bytes()) {
            let upper_result = headers.get(&name.to_ascii_uppercase());
            let lower_result = headers.get(&name.to_ascii_lowercase());
            let original_result = headers.get(&name);

            // All case variants should return the same value
            prop_assert_eq!(upper_result, original_result,
                "Upper-case lookup differs for header: {}", name);
            prop_assert_eq!(lower_result, original_result,
                "Lower-case lookup differs for header: {}", name);
        }
    }

    #[test]
    fn prop_headers_iterator_count_consistency(
        names in prop::collection::vec(r"[A-Za-z][A-Za-z0-9-]{0,10}", 1..5),
        values in prop::collection::vec(r"[A-Za-z0-9 ]{1,20}", 1..5)
    ) {
        // Build a header block with min(names, values) headers
        let count = names.len().min(values.len());
        let mut raw = String::new();
        for i in 0..count {
            raw.push_str(&format!("{}: {}\r\n", names[i], values[i]));
        }

        if let Ok(headers) = Headers::parse(raw.as_bytes()) {
            let iter_count = headers.iter().count();
            prop_assert_eq!(iter_count, count,
                "Iterator count {} should match header count {} for:\n{}",
                iter_count, count, raw);
        }
    }
}

// =============================================================================
// 9. CacheableStatusCode - TryFrom exhaustiveness
// =============================================================================

use nntp_proxy::cache::CacheableStatusCode;

proptest! {
    #[test]
    fn prop_cacheable_status_code_roundtrip(
        variant in prop::sample::select(vec![
            CacheableStatusCode::Article,
            CacheableStatusCode::Head,
            CacheableStatusCode::Body,
            CacheableStatusCode::Stat,
            CacheableStatusCode::Missing,
        ])
    ) {
        // Every valid variant roundtrips: as_u16 -> try_from -> same variant
        let raw = variant.as_u16();
        let back = CacheableStatusCode::try_from(raw);
        prop_assert!(back.is_ok(),
            "Roundtrip failed for variant {:?} (code {})", variant, raw);
        prop_assert_eq!(back.unwrap(), variant,
            "Roundtrip produced different variant for code {}", raw);
    }

    #[test]
    fn prop_cacheable_status_code_rejects_invalid(code in any::<u16>()) {
        let valid_codes = [220u16, 221, 222, 223, 430];
        let result = CacheableStatusCode::try_from(code);

        if valid_codes.contains(&code) {
            prop_assert!(result.is_ok(),
                "Valid code {} should succeed", code);
        } else {
            prop_assert!(result.is_err(),
                "Invalid code {} should be rejected", code);
            prop_assert_eq!(result.unwrap_err(), code,
                "Error should carry the rejected code {}", code);
        }
    }

    #[test]
    fn prop_cacheable_status_code_all_u16_handled(code in any::<u16>()) {
        // Every u16 value is handled: either success or error, never panic
        let result = CacheableStatusCode::try_from(code);
        prop_assert!(result.is_ok() || result.is_err(),
            "Code {} must be either Ok or Err", code);
    }
}

// =============================================================================
// 10. BackendSelector load tracking - Router state properties
// =============================================================================

proptest! {
    #[test]
    fn prop_backend_selector_add_increments_count(
        num_backends in 1usize..=8
    ) {
        let mut selector = nntp_proxy::router::BackendSelector::new();

        for i in 0..num_backends {
            let id = nntp_proxy::types::BackendId::from_index(i);
            let provider = nntp_proxy::pool::DeadpoolConnectionProvider::new(
                "localhost".to_string(), 119, format!("test-{}", i), 10, None, None
            );
            selector.add_backend(
                id,
                nntp_proxy::types::ServerName::try_new(format!("server-{}", i)).unwrap(),
                provider,
                0,
            );

            prop_assert_eq!(selector.backend_count().get(), i + 1,
                "Backend count should be {} after adding {} backends", i + 1, i + 1);
        }
    }

    #[test]
    fn prop_backend_selector_select_empty_returns_error(
        _dummy in 0..1u8  // proptest requires at least one parameter
    ) {
        let selector = nntp_proxy::router::BackendSelector::new();
        let client_id = nntp_proxy::types::ClientId::new();

        // Routing with zero backends should return an error
        let result = selector.route_command(client_id, "LIST");
        prop_assert!(result.is_err(),
            "route_command with zero backends should return error");
    }

    #[test]
    fn prop_backend_selector_distribution_fair(
        num_requests in 100usize..=200
    ) {
        // Set up 2 backends with equal weight
        let mut selector = nntp_proxy::router::BackendSelector::new();
        let id0 = nntp_proxy::types::BackendId::from_index(0);
        let id1 = nntp_proxy::types::BackendId::from_index(1);

        let provider0 = nntp_proxy::pool::DeadpoolConnectionProvider::new(
            "localhost".to_string(), 119, "test-0".to_string(), 10, None, None
        );
        let provider1 = nntp_proxy::pool::DeadpoolConnectionProvider::new(
            "localhost".to_string(), 119, "test-1".to_string(), 10, None, None
        );

        selector.add_backend(
            id0,
            nntp_proxy::types::ServerName::try_new("server-0".to_string()).unwrap(),
            provider0, 0,
        );
        selector.add_backend(
            id1,
            nntp_proxy::types::ServerName::try_new("server-1".to_string()).unwrap(),
            provider1, 0,
        );

        // Route many commands and count distribution
        let mut count0 = 0usize;
        let mut count1 = 0usize;
        for _ in 0..num_requests {
            let client_id = nntp_proxy::types::ClientId::new();
            if let Ok(id) = selector.route_command(client_id, "LIST") {
                if id == id0 { count0 += 1; }
                if id == id1 { count1 += 1; }
                // Complete immediately so pending counts don't accumulate
                selector.complete_command(id);
            }
        }

        let total = count0 + count1;
        prop_assert_eq!(total, num_requests,
            "All requests should be routed");

        // With equal weights, each backend should get roughly 50% of requests
        // Allow 30% tolerance for small sample sizes
        let min_expected = (num_requests as f64 * 0.20) as usize;
        prop_assert!(count0 >= min_expected,
            "Backend 0 got {} out of {} requests, expected at least {}",
            count0, num_requests, min_expected);
        prop_assert!(count1 >= min_expected,
            "Backend 1 got {} out of {} requests, expected at least {}",
            count1, num_requests, min_expected);
    }
}
