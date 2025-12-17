//! Cache policy decisions
//!
//! Pure functions for determining caching behavior based on response codes
//! and command types.

use crate::command::NntpCommand;

/// Determine what caching action to take for a response
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheAction {
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
/// Only full article responses (220) should be cached.
/// Response code 220 uniquely identifies ARTICLE command success:
/// - 220 = ARTICLE (full article - cache this)
/// - 221 = HEAD (headers only)
/// - 222 = BODY (body only)
/// - 223 = STAT (status only)
#[inline]
pub(crate) fn should_capture_for_cache(
    response_code: u16,
    is_multiline: bool,
    cache_articles: bool,
    has_message_id: bool,
) -> bool {
    cache_articles && is_multiline && has_message_id && response_code == 220
}

/// Check if a response should be tracked for availability (HEAD/BODY/ARTICLE/STAT success)
#[inline]
pub(crate) fn should_track_availability(response_code: u16, has_message_id: bool) -> bool {
    has_message_id && matches!(response_code, 220..=223)
}

/// Determine caching action for a response
///
/// Response codes uniquely identify the command type:
/// - 220 = ARTICLE (cache full article if enabled)
/// - 221 = HEAD (track availability)
/// - 222 = BODY (track availability)
/// - 223 = STAT (track availability)
///
/// The command is validated to ensure it's not a stateful command
/// that would require mode switching (GROUP, NEXT, XOVER, etc.)
pub(crate) fn determine_cache_action(
    command: &str,
    response_code: u16,
    is_multiline: bool,
    cache_articles: bool,
    has_message_id: bool,
) -> CacheAction {
    // Defensive check: don't cache responses from stateful commands
    // (These should have triggered mode switch, so this shouldn't happen)
    if NntpCommand::parse(command).is_stateful() {
        return CacheAction::None;
    }

    if !has_message_id {
        return CacheAction::None;
    }

    // Full article caching only for 220 response (ARTICLE command)
    if should_capture_for_cache(response_code, is_multiline, cache_articles, has_message_id) {
        CacheAction::CaptureArticle
    } else if is_multiline && should_track_availability(response_code, has_message_id) {
        // Track availability for HEAD (221), BODY (222), and ARTICLE (220) when cache disabled
        CacheAction::TrackAvailability
    } else if response_code == 223 {
        // STAT response - not multiline but track availability
        CacheAction::TrackStat
    } else {
        CacheAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests for should_capture_for_cache

    #[test]
    fn test_should_capture_for_cache_article_response() {
        // 220 response (ARTICLE) with all conditions met should capture
        assert!(should_capture_for_cache(220, true, true, true));

        // 221 (HEAD) and 222 (BODY) should NOT capture full article
        assert!(!should_capture_for_cache(221, true, true, true));
        assert!(!should_capture_for_cache(222, true, true, true));
    }

    #[test]
    fn test_should_capture_for_cache_requires_all_conditions() {
        // Not multiline
        assert!(!should_capture_for_cache(220, false, true, true));

        // Cache disabled
        assert!(!should_capture_for_cache(220, true, false, true));

        // No message-ID
        assert!(!should_capture_for_cache(220, true, true, false));

        // Wrong response code
        assert!(!should_capture_for_cache(430, true, true, true));
    }

    #[test]
    fn test_should_capture_for_cache_only_220() {
        // Only 220 (ARTICLE) responses should be captured
        assert!(should_capture_for_cache(220, true, true, true));
        assert!(!should_capture_for_cache(221, true, true, true)); // HEAD
        assert!(!should_capture_for_cache(222, true, true, true)); // BODY
        assert!(!should_capture_for_cache(223, true, true, true)); // STAT
    }

    // Tests for should_track_availability

    #[test]
    fn test_should_track_availability_success_responses() {
        assert!(should_track_availability(220, true)); // ARTICLE
        assert!(should_track_availability(221, true)); // HEAD
        assert!(should_track_availability(222, true)); // BODY
        assert!(should_track_availability(223, true)); // STAT
    }

    #[test]
    fn test_should_track_availability_requires_message_id() {
        assert!(!should_track_availability(220, false));
        assert!(!should_track_availability(223, false));
    }

    #[test]
    fn test_should_track_availability_error_responses() {
        assert!(!should_track_availability(430, true)); // Article not found
        assert!(!should_track_availability(500, true)); // Server error
        assert!(!should_track_availability(200, true)); // Greeting
    }

    // Tests for determine_cache_action

    #[test]
    fn test_determine_cache_action_capture_article() {
        // Full article capture for 220 response when cache enabled
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 220, true, true, true),
            CacheAction::CaptureArticle
        );
    }

    #[test]
    fn test_determine_cache_action_track_availability() {
        // Track availability for HEAD (221) and BODY (222) responses
        assert_eq!(
            determine_cache_action("HEAD <test@example.com>", 221, true, true, true),
            CacheAction::TrackAvailability
        );
        assert_eq!(
            determine_cache_action("BODY <test@example.com>", 222, true, true, true),
            CacheAction::TrackAvailability
        );
    }

    #[test]
    fn test_determine_cache_action_track_stat() {
        // Track STAT (223) - not multiline
        assert_eq!(
            determine_cache_action("STAT <test@example.com>", 223, false, false, true),
            CacheAction::TrackStat
        );
    }

    #[test]
    fn test_determine_cache_action_no_message_id() {
        // No caching without message-ID
        assert_eq!(
            determine_cache_action("ARTICLE 123", 220, true, true, false),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("STAT 123", 223, false, false, false),
            CacheAction::None
        );
    }

    #[test]
    fn test_determine_cache_action_error_responses() {
        // No caching for error responses
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 430, true, true, true),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 500, true, true, true),
            CacheAction::None
        );
    }

    #[test]
    fn test_determine_cache_action_cache_disabled() {
        // When cache_articles is false, don't capture full article but still track availability
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 220, true, false, true),
            CacheAction::TrackAvailability
        );
    }

    #[test]
    fn test_determine_cache_action_rejects_stateful_commands() {
        // Stateful commands should never reach cache action determination,
        // but if they do, we defensively reject them
        assert_eq!(
            determine_cache_action("GROUP alt.test", 211, false, true, false),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("XOVER 1-100", 224, true, true, true),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("NEXT", 223, false, true, false),
            CacheAction::None
        );
    }
}
