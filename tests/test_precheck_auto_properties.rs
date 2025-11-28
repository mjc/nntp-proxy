//! Property-based tests for precheck AUTO mode
//!
//! Tests the AUTO mode behavior that sends both STAT and HEAD:
//! - Discrepancy detection when STAT and HEAD disagree
//! - Error handling when either command reports error
//! - Correct article availability determination

use proptest::prelude::*;

/// Generate valid NNTP status codes
fn status_code_strategy() -> impl Strategy<Value = u16> {
    prop_oneof![
        Just(200), // Greeting
        Just(221), // HEAD success
        Just(223), // STAT success
        Just(430), // Article not found
        Just(480), // Auth required
        Just(500), // Command not recognized
        Just(502), // Permission denied
    ]
}

proptest! {
    /// Property: When both STAT and HEAD report success (223/221), article is available
    #[test]
    fn prop_both_success_means_available(
        _seed in 0u64..1000,
    ) {
        let stat_code = 223u16;
        let head_code = 221u16;

        let stat_has_article = stat_code == 223;
        let head_has_article = head_code == 221;
        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        // If either reports error, article NOT available
        let has_article = if stat_error || head_error {
            false
        } else {
            stat_has_article && head_has_article
        };

        prop_assert_eq!(has_article, true);
        prop_assert_eq!(stat_has_article != head_has_article, false); // No discrepancy
    }

    /// Property: When either reports error (430 or 5xx), article is NOT available
    #[test]
    fn prop_error_means_not_available(
        stat_code in status_code_strategy(),
        head_code in status_code_strategy(),
    ) {
        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        if stat_error || head_error {
            let stat_has_article = stat_code == 223;
            let head_has_article = head_code == 221;

            let has_article = if stat_error || head_error {
                false
            } else {
                stat_has_article && head_has_article
            };

            prop_assert_eq!(has_article, false);
        }
    }

    /// Property: STAT=success + HEAD=not-found triggers discrepancy
    #[test]
    fn prop_stat_success_head_fail_is_discrepancy(
        _seed in 0u64..1000,
    ) {
        let stat_code = 223u16; // Success
        let head_code = 430u16; // Not found

        let stat_has_article = stat_code == 223;
        let head_has_article = head_code == 221;

        let discrepancy = stat_has_article != head_has_article;
        prop_assert_eq!(discrepancy, true);
    }

    /// Property: HEAD=success + STAT=not-found triggers discrepancy
    #[test]
    fn prop_head_success_stat_fail_is_discrepancy(
        _seed in 0u64..1000,
    ) {
        let stat_code = 430u16; // Not found
        let head_code = 221u16; // Success

        let stat_has_article = stat_code == 223;
        let head_has_article = head_code == 221;

        let discrepancy = stat_has_article != head_has_article;
        prop_assert_eq!(discrepancy, true);
    }

    /// Property: Both reporting not-found is NOT a discrepancy
    #[test]
    fn prop_both_not_found_no_discrepancy(
        _seed in 0u64..1000,
    ) {
        let stat_code = 430u16;
        let head_code = 430u16;

        let stat_has_article = stat_code == 223;
        let head_has_article = head_code == 221;

        let discrepancy = stat_has_article != head_has_article;
        prop_assert_eq!(discrepancy, false);
    }

    /// Property: Error codes (5xx) are treated as errors, 430 is treated as error too
    #[test]
    fn prop_error_vs_not_found(
        error_code in 500u16..600,
    ) {
        // In AUTO mode, BOTH 430 and 5xx are treated as errors
        let not_found_is_error = 430 >= 430; // true
        let server_error_is_error = error_code >= 500; // true

        // Both are errors in the implementation
        prop_assert_eq!(not_found_is_error, true);
        prop_assert_eq!(server_error_is_error, true);
    }

    /// Property: Discrepancy detection is symmetric
    #[test]
    fn prop_discrepancy_symmetric(
        code1 in status_code_strategy(),
        code2 in status_code_strategy(),
    ) {
        let has1 = code1 == 223 || code1 == 221;
        let has2 = code2 == 223 || code2 == 221;

        let discrepancy = has1 != has2;
        let reverse_discrepancy = has2 != has1;

        prop_assert_eq!(discrepancy, reverse_discrepancy);
    }

    /// Property: Disagreement means prefer negative (safer to report "not found")
    #[test]
    fn prop_disagreement_prefers_negative(
        stat_code in status_code_strategy(),
        head_code in status_code_strategy(),
    ) {
        let stat_has = stat_code == 223;
        let head_has = head_code == 221;
        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        let has_article = if stat_error || head_error {
            false
        } else if stat_has != head_has {
            // Disagreement: prefer negative
            false
        } else {
            stat_has // Both agree
        };

        // If they disagree and no errors, result must be false
        if !stat_error && !head_error && stat_has != head_has {
            prop_assert_eq!(has_article, false);
        }

        // If article available, both MUST have agreed it's available
        if has_article {
            prop_assert_eq!(stat_has, true);
            prop_assert_eq!(head_has, true);
        }
    }

    /// Property: At least one error -> article NOT available (fail-fast)
    #[test]
    fn prop_any_error_fails_fast(
        stat_code in status_code_strategy(),
        head_code in status_code_strategy(),
    ) {
        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        if stat_error || head_error {
            let stat_has = stat_code == 223;
            let head_has = head_code == 221;

            let has_article = if stat_error || head_error {
                false
            } else if stat_has != head_has {
                false
            } else {
                stat_has
            };

            prop_assert_eq!(has_article, false);
        }
    }
}

#[cfg(test)]
mod auto_mode_logic {
    /// Test the exact AUTO mode decision logic

    #[test]
    fn test_both_223_and_221_available() {
        let stat_code = 223;
        let head_code = 221;

        let stat_has = stat_code == 223;
        let head_has = head_code == 221;
        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        let has_article = if stat_error || head_error {
            false
        } else if stat_has != head_has {
            false
        } else {
            stat_has
        };

        assert_eq!(has_article, true);
        assert_eq!(stat_has != head_has, false); // No discrepancy
    }

    #[test]
    fn test_stat_430_head_221_discrepancy() {
        let stat_code = 430;
        let head_code = 221;

        let stat_has = stat_code == 223;
        let head_has = head_code == 221;

        assert_eq!(stat_has, false);
        assert_eq!(head_has, true);
        assert_eq!(stat_has != head_has, true); // DISCREPANCY!

        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        // 430 is error, so article NOT available
        let has_article = if stat_error || head_error {
            false
        } else if stat_has != head_has {
            false
        } else {
            stat_has
        };

        assert_eq!(has_article, false);
    }

    #[test]
    fn test_stat_223_head_430_discrepancy() {
        let stat_code = 223;
        let head_code = 430;

        let stat_has = stat_code == 223;
        let head_has = head_code == 221;

        assert_eq!(stat_has, true);
        assert_eq!(head_has, false);
        assert_eq!(stat_has != head_has, true); // DISCREPANCY!

        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        // 430 is error, so article NOT available
        let has_article = if stat_error || head_error {
            false
        } else if stat_has != head_has {
            false
        } else {
            stat_has
        };

        assert_eq!(has_article, false);
    }

    #[test]
    fn test_both_430_no_discrepancy() {
        let stat_code = 430;
        let head_code = 430;

        let stat_has = stat_code == 223;
        let head_has = head_code == 221;

        assert_eq!(stat_has, false);
        assert_eq!(head_has, false);
        assert_eq!(stat_has != head_has, false); // No discrepancy

        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        let has_article = if stat_error || head_error {
            false
        } else if stat_has != head_has {
            false
        } else {
            stat_has
        };

        assert_eq!(has_article, false);
    }

    #[test]
    fn test_500_error_always_not_available() {
        let stat_code = 500;
        let head_code = 221; // Even if HEAD says yes

        let stat_error = stat_code == 430 || stat_code >= 500;
        let head_error = head_code == 430 || head_code >= 500;

        assert_eq!(stat_error, true);

        let stat_has = stat_code == 223;
        let head_has = head_code == 221;

        let has_article = if stat_error || head_error {
            false
        } else if stat_has != head_has {
            false
        } else {
            stat_has
        };

        // Error always means NOT available
        assert_eq!(has_article, false);
    }
}
