//! Metrics recording policy
//!
//! Pure functions for determining what metrics to record for a typed request response.

use crate::protocol::{RequestContext, RequestKind, StatusCode};

/// Determine what action to take for metrics recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsAction {
    /// Record 4xx error (excluding 423, 430)
    Error4xx,
    /// Record 5xx error
    Error5xx,
    /// Record article metrics for article-like response body responses.
    Article,
    /// Record an authoritative `430`. Counted on its own so a missing article
    /// is never confused with a refused command, and still added to the 4xx
    /// totals the dashboard shows.
    ArticleMissing,
    /// No special recording needed
    None,
}

/// Determine metrics recording action for a typed request and status code.
///
/// # Response Code Classification
/// - 430 → record as an authoritative missing article
/// - 4xx errors (excluding 423, 430) → record as errors
/// - 5xx errors → record as errors
/// - ARTICLE/HEAD/BODY success payload responses → record as articles
/// - Everything else → no special action
///
/// # Excluded Error Codes
/// - 423 (no such article number in group) - expected for article numbers
/// - 430 (no such article) - its own [`MetricsAction::ArticleMissing`]
pub fn determine_metrics_action_for_request(
    request: &RequestContext,
    status_code: StatusCode,
) -> MetricsAction {
    determine_metrics_action_for_kind(request.kind(), status_code)
}

/// Same classification from the request kind alone, for paths such as the
/// stateful reply tracker that keep the kind but not the whole request.
#[must_use]
pub fn determine_metrics_action_for_kind(
    kind: RequestKind,
    status_code: StatusCode,
) -> MetricsAction {
    let response_code = status_code.as_u16();

    if status_code.is_article_missing() {
        MetricsAction::ArticleMissing
    } else if (400..500).contains(&response_code) && response_code != 423 {
        MetricsAction::Error4xx
    } else if response_code >= 500 {
        MetricsAction::Error5xx
    } else if crate::protocol::request_kind_has_response_body(kind, status_code)
        && matches!(response_code, 220..=222)
    {
        MetricsAction::Article
    } else {
        MetricsAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn determine_metrics_action(command: &str, response_code: u16) -> MetricsAction {
        let request = RequestContext::parse(command.as_bytes()).expect("valid request line");
        determine_metrics_action_for_request(&request, StatusCode::new(response_code))
    }

    #[test]
    fn test_determine_metrics_action_4xx_errors() {
        // 4xx errors (excluding 423, 430) should record error_4xx
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 400),
            MetricsAction::Error4xx
        );
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 401),
            MetricsAction::Error4xx
        );
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 480),
            MetricsAction::Error4xx
        );
    }

    #[test]
    fn test_determine_metrics_action_4xx_exclusions() {
        // 423 (no such article number) and 430 (no such article) are not errors
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 423),
            MetricsAction::None
        );
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 430),
            MetricsAction::ArticleMissing
        );
    }

    #[test]
    fn test_determine_metrics_action_5xx_errors() {
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 500),
            MetricsAction::Error5xx
        );
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 502),
            MetricsAction::Error5xx
        );
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 503),
            MetricsAction::Error5xx
        );
    }

    #[test]
    fn test_determine_metrics_action_article_success() {
        // Article-like success payload responses should record article metrics.
        assert_eq!(
            determine_metrics_action("ARTICLE <test@example.com>", 220),
            MetricsAction::Article
        );
        assert_eq!(
            determine_metrics_action("HEAD <test@example.com>", 221),
            MetricsAction::Article
        );
        assert_eq!(
            determine_metrics_action("BODY <test@example.com>", 222),
            MetricsAction::Article
        );
    }

    #[test]
    fn test_determine_metrics_action_article_status_not_for_other_request() {
        // The status code alone does not decide response shape.
        assert_eq!(
            determine_metrics_action("GROUP alt.test", 220),
            MetricsAction::None
        );
    }

    #[test]
    fn test_determine_metrics_action_stat_not_article() {
        assert_eq!(
            determine_metrics_action("STAT <test@example.com>", 223),
            MetricsAction::None
        );
    }

    #[test]
    fn test_determine_metrics_action_success_codes() {
        // 2xx success codes (other than 220-222) should not record article
        assert_eq!(
            determine_metrics_action("MODE READER", 200),
            MetricsAction::None
        );
        assert_eq!(
            determine_metrics_action("GROUP alt.test", 211),
            MetricsAction::None
        );
        assert_eq!(determine_metrics_action("LIST", 215), MetricsAction::None);
        assert_eq!(
            determine_metrics_action("LISTGROUP alt.test", 211),
            MetricsAction::None
        );
    }

    #[test]
    fn test_determine_metrics_action_3xx_codes() {
        // 3xx codes should not record anything special
        assert_eq!(
            determine_metrics_action("IHAVE <test@example.com>", 335),
            MetricsAction::None
        );
        assert_eq!(
            determine_metrics_action("AUTHINFO USER user", 381),
            MetricsAction::None
        );
    }
}
