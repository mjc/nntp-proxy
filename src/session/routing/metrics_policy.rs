//! Metrics recording policy
//!
//! Pure functions for determining what metrics to record based on response codes.

/// Determine what action to take for metrics recording based on response code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetricsAction {
    /// Record 4xx error (excluding 423, 430)
    Error4xx,
    /// Record 5xx error
    Error5xx,
    /// Record article metrics (success response for multiline)
    Article,
    /// No special recording needed
    None,
}

/// Determine metrics recording action based on response code
///
/// # Response Code Classification
/// - 4xx errors (excluding 423, 430) → record as errors
/// - 5xx errors → record as errors
/// - 220-222 multiline → record as articles
/// - Everything else → no special action
///
/// # Excluded Error Codes
/// - 423 (no such article number in group) - expected for article numbers
/// - 430 (no such article) - expected, handled by retry logic
pub(crate) fn determine_metrics_action(response_code: u16, is_multiline: bool) -> MetricsAction {
    if (400..500).contains(&response_code) && response_code != 423 && response_code != 430 {
        MetricsAction::Error4xx
    } else if response_code >= 500 {
        MetricsAction::Error5xx
    } else if is_multiline && matches!(response_code, 220..=222) {
        MetricsAction::Article
    } else {
        MetricsAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_metrics_action_4xx_errors() {
        // 4xx errors (excluding 423, 430) should record error_4xx
        assert_eq!(
            determine_metrics_action(400, false),
            MetricsAction::Error4xx
        );
        assert_eq!(
            determine_metrics_action(401, false),
            MetricsAction::Error4xx
        );
        assert_eq!(
            determine_metrics_action(480, false),
            MetricsAction::Error4xx
        );
    }

    #[test]
    fn test_determine_metrics_action_4xx_exclusions() {
        // 423 (no such article number) and 430 (no such article) are not errors
        assert_eq!(determine_metrics_action(423, false), MetricsAction::None);
        assert_eq!(determine_metrics_action(430, false), MetricsAction::None);
    }

    #[test]
    fn test_determine_metrics_action_5xx_errors() {
        assert_eq!(
            determine_metrics_action(500, false),
            MetricsAction::Error5xx
        );
        assert_eq!(
            determine_metrics_action(502, false),
            MetricsAction::Error5xx
        );
        assert_eq!(
            determine_metrics_action(503, false),
            MetricsAction::Error5xx
        );
    }

    #[test]
    fn test_determine_metrics_action_article_success() {
        // Multiline 220-222 should record article
        assert_eq!(determine_metrics_action(220, true), MetricsAction::Article);
        assert_eq!(determine_metrics_action(221, true), MetricsAction::Article);
        assert_eq!(determine_metrics_action(222, true), MetricsAction::Article);
    }

    #[test]
    fn test_determine_metrics_action_article_not_multiline() {
        // Non-multiline shouldn't record article
        assert_eq!(determine_metrics_action(220, false), MetricsAction::None);
    }

    #[test]
    fn test_determine_metrics_action_stat_not_article() {
        // STAT (223) is not an article even if multiline flag is true
        assert_eq!(determine_metrics_action(223, true), MetricsAction::None);
    }

    #[test]
    fn test_determine_metrics_action_success_codes() {
        // 2xx success codes (other than 220-222) should not record article
        assert_eq!(determine_metrics_action(200, false), MetricsAction::None);
        assert_eq!(determine_metrics_action(211, false), MetricsAction::None);
        assert_eq!(determine_metrics_action(215, true), MetricsAction::None);
    }

    #[test]
    fn test_determine_metrics_action_3xx_codes() {
        // 3xx codes should not record anything special
        assert_eq!(determine_metrics_action(335, false), MetricsAction::None);
        assert_eq!(determine_metrics_action(381, false), MetricsAction::None);
    }
}
