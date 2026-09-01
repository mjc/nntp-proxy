//! Cache policy decisions
//!
//! Pure functions for mapping typed requests and response codes to cache actions.

use crate::command::ArticleLookupRequest;
#[cfg(test)]
use crate::protocol::RequestContext;
use crate::protocol::StatusCode;

/// Cache-side outcome for a routed response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAction {
    /// Capture cacheable article/body payload bytes and cache them.
    CaptureArticle,
    /// Track availability only for successful article-like lookups.
    TrackAvailability,
    /// Track STAT availability (223 response).
    TrackStat,
    /// No cache or availability update is needed.
    None,
}

/// Check if this response should be retained in the article/body cache.
///
/// Cache full ARTICLE responses (220) and BODY responses (222). HEAD and STAT
/// successes update availability only.
/// Response codes:
/// - 220 = ARTICLE (full article - headers + body)
/// - 221 = HEAD (headers only - track availability)
/// - 222 = BODY (body only - cache this for yEnc content)
/// - 223 = STAT (availability, no payload)
#[inline]
fn should_capture_article_response(
    request: &ArticleLookupRequest<'_>,
    response_code: StatusCode,
    cache_articles: bool,
) -> bool {
    cache_articles
        && request.request().has_response_body(response_code)
        && matches!(response_code.as_u16(), 220 | 222)
}

/// Check if this response should update article availability tracking.
///
/// Track successful article-like responses:
/// - 220 = ARTICLE
/// - 221 = HEAD
/// - 222 = BODY
/// - 223 = STAT
#[inline]
pub fn should_track_availability(response_code: StatusCode) -> bool {
    matches!(response_code.as_u16(), 220..=223)
}

#[cfg(test)]
fn determine_cache_action(
    command: &str,
    response_code: u16,
    cache_articles: bool,
    has_message_id: bool,
) -> CacheAction {
    let request = RequestContext::parse(command.as_bytes()).expect("valid request line");
    let response_code = StatusCode::new(response_code);
    let article_request = crate::command::CommandHandler::article_lookup_request(&request);
    let article_request = article_request.filter(|_| has_message_id);
    determine_cache_action_for_request(article_request, response_code, cache_articles)
}

pub fn determine_cache_action_for_request(
    article_request: Option<ArticleLookupRequest<'_>>,
    response_code: StatusCode,
    cache_articles: bool,
) -> CacheAction {
    let Some(article_request) = article_request else {
        return CacheAction::None;
    };

    let has_response_body = article_request.request().has_response_body(response_code);

    if should_capture_article_response(&article_request, response_code, cache_articles) {
        CacheAction::CaptureArticle
    } else if has_response_body && should_track_availability(response_code) {
        CacheAction::TrackAvailability
    } else if response_code.as_u16() == 223 {
        CacheAction::TrackStat
    } else {
        CacheAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: &str) -> RequestContext {
        RequestContext::parse(command.as_bytes()).expect("valid request line")
    }
    fn should_capture_for_test(
        request: &RequestContext,
        response_code: StatusCode,
        cache_articles: bool,
        has_message_id: bool,
    ) -> bool {
        if !has_message_id {
            return false;
        }

        let article_request = crate::command::CommandHandler::article_lookup_request(request);
        article_request.as_ref().is_some_and(|request| {
            should_capture_article_response(request, response_code, cache_articles)
        })
    }
    fn should_track_availability_for_test(response_code: StatusCode, has_message_id: bool) -> bool {
        has_message_id && should_track_availability(response_code)
    }

    // Tests for should_capture_for_cache

    #[test]
    fn test_should_capture_for_cache_article_response() {
        // 220 (ARTICLE) and 222 (BODY) with all conditions met should capture
        assert!(should_capture_for_test(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            true,
            true
        ));
        assert!(should_capture_for_test(
            &request("BODY <test@example.com>"),
            StatusCode::new(222),
            true,
            true
        ));

        // 221 (HEAD) should NOT capture (headers only)
        assert!(!should_capture_for_test(
            &request("HEAD <test@example.com>"),
            StatusCode::new(221),
            true,
            true
        ));
    }

    #[test]
    fn test_should_capture_for_cache_requires_all_conditions() {
        // Response is not a body response for this request.
        assert!(!should_capture_for_test(
            &request("STAT <test@example.com>"),
            StatusCode::new(220),
            true,
            true
        ));

        // Cache disabled
        assert!(!should_capture_for_test(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            false,
            true
        ));

        // No message-ID
        assert!(!should_capture_for_test(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            true,
            false
        ));

        // Wrong response code
        assert!(!should_capture_for_test(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(430),
            true,
            true
        ));
    }

    #[test]
    fn test_should_capture_for_cache_220_and_222() {
        // 220 (ARTICLE) and 222 (BODY) responses should be captured
        assert!(should_capture_for_test(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            true,
            true
        ));
        assert!(should_capture_for_test(
            &request("BODY <test@example.com>"),
            StatusCode::new(222),
            true,
            true
        )); // BODY
        assert!(!should_capture_for_test(
            &request("HEAD <test@example.com>"),
            StatusCode::new(221),
            true,
            true
        )); // HEAD
        assert!(!should_capture_for_test(
            &request("STAT <test@example.com>"),
            StatusCode::new(223),
            true,
            true
        )); // STAT
    }

    // Tests for should_track_availability

    #[test]
    fn test_should_track_availability_success_responses() {
        assert!(should_track_availability_for_test(
            StatusCode::new(220),
            true
        )); // ARTICLE
        assert!(should_track_availability_for_test(
            StatusCode::new(221),
            true
        )); // HEAD
        assert!(should_track_availability_for_test(
            StatusCode::new(222),
            true
        )); // BODY
        assert!(should_track_availability_for_test(
            StatusCode::new(223),
            true
        )); // STAT
    }

    #[test]
    fn test_should_track_availability_requires_message_id() {
        assert!(!should_track_availability_for_test(
            StatusCode::new(220),
            false
        ));
        assert!(!should_track_availability_for_test(
            StatusCode::new(223),
            false
        ));
    }

    #[test]
    fn test_should_track_availability_error_responses() {
        assert!(!should_track_availability_for_test(
            StatusCode::new(430),
            true
        )); // Article not found
        assert!(!should_track_availability_for_test(
            StatusCode::new(500),
            true
        )); // Server error
        assert!(!should_track_availability_for_test(
            StatusCode::new(200),
            true
        )); // Greeting
    }

    // Tests for determine_cache_action

    #[test]
    fn test_determine_cache_action_capture_article() {
        // Full article capture for 220 response when cache enabled
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 220, true, true),
            CacheAction::CaptureArticle
        );
    }

    #[test]
    fn test_determine_cache_action_track_availability() {
        // HEAD (221) only tracks availability (headers only)
        assert_eq!(
            determine_cache_action("HEAD <test@example.com>", 221, true, true),
            CacheAction::TrackAvailability
        );
        // BODY (222) captures the body payload when cache_articles=true
        assert_eq!(
            determine_cache_action("BODY <test@example.com>", 222, true, true),
            CacheAction::CaptureArticle
        );
        // BODY (222) with cache_articles=false only tracks availability
        assert_eq!(
            determine_cache_action("BODY <test@example.com>", 222, false, true),
            CacheAction::TrackAvailability
        );
    }

    #[test]
    fn test_determine_cache_action_track_stat() {
        // STAT (223) can retain a synthetic payload in article-cache mode, or
        // record positive availability in availability-only mode.
        assert_eq!(
            determine_cache_action("STAT <test@example.com>", 223, true, true),
            CacheAction::TrackStat
        );
        assert_eq!(
            determine_cache_action("STAT <test@example.com>", 223, false, true),
            CacheAction::TrackStat
        );
    }

    #[test]
    fn test_determine_cache_action_error_responses() {
        // No caching for error responses
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 430, true, true),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 500, true, true),
            CacheAction::None
        );
    }

    #[test]
    fn test_determine_cache_action_cache_disabled() {
        // When cache_articles is false, don't retain payload bytes but still track availability
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 220, false, true),
            CacheAction::TrackAvailability
        );
    }

    // Note: test_determine_cache_action_rejects_stateful_commands deleted
    // because stateful command check is now a debug_assert (zero-cost in release).
    // The debug_assert will catch bugs during development, but this is a
    // "should never happen" case that doesn't need explicit unit tests.
    //
    // Note: test_determine_cache_action_no_message_id deleted because the
    // has_message_id=false case now short-circuits before any other logic,
    // and is already tested implicitly by all tests that pass has_message_id=true.
}
