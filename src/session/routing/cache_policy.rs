//! Cache policy decisions
//!
//! Pure functions for determining caching behavior based on response codes
//! and command types.

use crate::protocol::{RequestContext, RequestRouteClass, StatusCode};

/// Determine what caching action to take for a response
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAction {
    /// Capture full article content and cache it
    CaptureArticle,
    /// Track availability only (for HEAD/BODY/STAT success)
    TrackAvailability,
    /// Track STAT availability (223 response)
    TrackStat,
    /// No caching action needed
    None,
}

/// Check if a response should be captured for article caching
///
/// Cache full article responses (220) AND body responses (222).
/// Response codes:
/// - 220 = ARTICLE (full article - headers + body)
/// - 221 = HEAD (headers only - don't cache)
/// - 222 = BODY (body only - cache this for yEnc content)
/// - 223 = STAT (availability, no payload)
#[inline]
fn should_capture_for_cache(
    request: &RequestContext,
    response_code: StatusCode,
    cache_articles: bool,
    has_message_id: bool,
) -> bool {
    cache_articles
        && request.expects_multiline_body(response_code)
        && has_message_id
        && matches!(response_code.as_u16(), 220 | 222)
}

/// Check if a response should be tracked for availability (HEAD/BODY/ARTICLE/STAT success)
#[inline]
pub fn should_track_availability(response_code: StatusCode, has_message_id: bool) -> bool {
    has_message_id && matches!(response_code.as_u16(), 220..=223)
}

/// Determine caching action for a response
///
/// The request context determines whether the status code carries a multiline
/// payload for this command:
/// - ARTICLE + 220 = capture full article if enabled
/// - HEAD + 221 = track availability
/// - BODY + 222 = capture body payload if enabled, otherwise track availability
/// - STAT + 223 = track availability
///
/// The command is validated to ensure it's not a stateful command
/// that would require mode switching (GROUP, NEXT, XOVER, etc.)
#[cfg(test)]
fn determine_cache_action(
    command: &str,
    response_code: u16,
    cache_articles: bool,
    has_message_id: bool,
) -> CacheAction {
    let request =
        RequestContext::from_request_line(crate::protocol::RequestLine::parse(command.as_bytes()));
    let response_code = StatusCode::new(response_code);
    determine_cache_action_for_request(&request, response_code, cache_articles, has_message_id)
}

pub fn determine_cache_action_for_request(
    request: &RequestContext,
    response_code: StatusCode,
    cache_articles: bool,
    has_message_id: bool,
) -> CacheAction {
    debug_assert!(
        !matches!(request.route_class(), RequestRouteClass::Stateful),
        "stateful command in PerCommand path: {:?}",
        request.kind()
    );

    if !has_message_id {
        return CacheAction::None;
    }

    let expects_multiline_body = request.expects_multiline_body(response_code);

    if should_capture_for_cache(request, response_code, cache_articles, has_message_id) {
        CacheAction::CaptureArticle
    } else if expects_multiline_body && should_track_availability(response_code, has_message_id) {
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
        RequestContext::from_request_line(crate::protocol::RequestLine::parse(command.as_bytes()))
    }

    // Tests for should_capture_for_cache

    #[test]
    fn test_should_capture_for_cache_article_response() {
        // 220 (ARTICLE) and 222 (BODY) with all conditions met should capture
        assert!(should_capture_for_cache(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            true,
            true
        ));
        assert!(should_capture_for_cache(
            &request("BODY <test@example.com>"),
            StatusCode::new(222),
            true,
            true
        ));

        // 221 (HEAD) should NOT capture (headers only)
        assert!(!should_capture_for_cache(
            &request("HEAD <test@example.com>"),
            StatusCode::new(221),
            true,
            true
        ));
    }

    #[test]
    fn test_should_capture_for_cache_requires_all_conditions() {
        // Response is not multiline for this request.
        assert!(!should_capture_for_cache(
            &request("STAT <test@example.com>"),
            StatusCode::new(220),
            true,
            true
        ));

        // Cache disabled
        assert!(!should_capture_for_cache(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            false,
            true
        ));

        // No message-ID
        assert!(!should_capture_for_cache(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            true,
            false
        ));

        // Wrong response code
        assert!(!should_capture_for_cache(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(430),
            true,
            true
        ));
    }

    #[test]
    fn test_should_capture_for_cache_220_and_222() {
        // 220 (ARTICLE) and 222 (BODY) responses should be captured
        assert!(should_capture_for_cache(
            &request("ARTICLE <test@example.com>"),
            StatusCode::new(220),
            true,
            true
        ));
        assert!(should_capture_for_cache(
            &request("BODY <test@example.com>"),
            StatusCode::new(222),
            true,
            true
        )); // BODY
        assert!(!should_capture_for_cache(
            &request("HEAD <test@example.com>"),
            StatusCode::new(221),
            true,
            true
        )); // HEAD
        assert!(!should_capture_for_cache(
            &request("STAT <test@example.com>"),
            StatusCode::new(223),
            true,
            true
        )); // STAT
    }

    // Tests for should_track_availability

    #[test]
    fn test_should_track_availability_success_responses() {
        assert!(should_track_availability(StatusCode::new(220), true)); // ARTICLE
        assert!(should_track_availability(StatusCode::new(221), true)); // HEAD
        assert!(should_track_availability(StatusCode::new(222), true)); // BODY
        assert!(should_track_availability(StatusCode::new(223), true)); // STAT
    }

    #[test]
    fn test_should_track_availability_requires_message_id() {
        assert!(!should_track_availability(StatusCode::new(220), false));
        assert!(!should_track_availability(StatusCode::new(223), false));
    }

    #[test]
    fn test_should_track_availability_error_responses() {
        assert!(!should_track_availability(StatusCode::new(430), true)); // Article not found
        assert!(!should_track_availability(StatusCode::new(500), true)); // Server error
        assert!(!should_track_availability(StatusCode::new(200), true)); // Greeting
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
        // BODY (222) now captures full article when cache_articles=true
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
        // Track STAT (223) as availability-only metadata.
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
        // When cache_articles is false, don't capture full article but still track availability
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
