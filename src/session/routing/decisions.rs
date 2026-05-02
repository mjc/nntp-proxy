//! Command routing decisions
//!
//! Pure functions for determining how to route NNTP commands based on
//! authentication state and routing mode.

use crate::config::RoutingMode;
use crate::protocol::{RequestContext, RequestKind, RequestRouteClass};

/// Decision for how to handle a command in per-command routing mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRoutingDecision {
    /// Intercept and handle authentication locally
    InterceptAuth,
    /// Return a synthetic proxy-accurate CAPABILITIES list
    InterceptCapabilities,
    /// Forward command to backend (authenticated or auth disabled)
    Forward,
    /// Require authentication first
    RequireAuth,
    /// Switch to stateful mode (hybrid mode only)
    SwitchToStateful,
    /// Reject the command
    Reject,
}

/// Determine how to handle a request based on auth state and routing mode
///
/// This is a pure function that can be easily tested without I/O dependencies.
#[must_use]
pub fn decide_request_routing(
    request: &RequestContext,
    is_authenticated: bool,
    auth_enabled: bool,
    routing_mode: RoutingMode,
) -> CommandRoutingDecision {
    match request.kind() {
        RequestKind::AuthInfo => CommandRoutingDecision::InterceptAuth,
        RequestKind::Capabilities => CommandRoutingDecision::InterceptCapabilities,
        RequestKind::Quit => CommandRoutingDecision::Forward,
        _ => match request.route_class() {
            RequestRouteClass::ArticleByMessageId | RequestRouteClass::Stateless => {
                if is_authenticated || !auth_enabled {
                    CommandRoutingDecision::Forward
                } else {
                    CommandRoutingDecision::RequireAuth
                }
            }
            RequestRouteClass::Stateful if routing_mode == RoutingMode::Hybrid => {
                CommandRoutingDecision::SwitchToStateful
            }
            RequestRouteClass::Stateful | RequestRouteClass::Reject => {
                CommandRoutingDecision::Reject
            }
            RequestRouteClass::Local => CommandRoutingDecision::Reject,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide_command_routing(
        command: &str,
        is_authenticated: bool,
        auth_enabled: bool,
        routing_mode: RoutingMode,
    ) -> CommandRoutingDecision {
        let request = RequestContext::from_request_bytes(command.as_bytes());
        decide_request_routing(&request, is_authenticated, auth_enabled, routing_mode)
    }

    #[test]
    fn test_decide_routing_auth_commands_always_intercepted() {
        // Auth commands should always be intercepted regardless of other flags
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", false, false, RoutingMode::Stateful),
            CommandRoutingDecision::InterceptAuth
        );
    }

    #[test]
    fn test_decide_routing_forward_when_authenticated() {
        // Should forward when authenticated, regardless of auth_enabled
        assert_eq!(
            decide_command_routing("LIST", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
        assert_eq!(
            decide_command_routing("LIST", true, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_forward_when_auth_disabled() {
        // Should forward when auth is disabled, even if not authenticated
        assert_eq!(
            decide_command_routing("LIST", false, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_require_auth_when_needed() {
        // Should require auth when auth is enabled but not authenticated
        assert_eq!(
            decide_command_routing("LIST", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RequireAuth
        );
    }

    #[test]
    fn test_decide_routing_switch_to_stateful_in_hybrid_mode() {
        // Hybrid mode with stateful command should switch to stateful
        assert_eq!(
            decide_command_routing("GROUP alt.test", true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::SwitchToStateful
        );

        // Also works when not authenticated
        assert_eq!(
            decide_command_routing("XOVER 1-100", false, false, RoutingMode::Hybrid),
            CommandRoutingDecision::SwitchToStateful
        );
    }

    #[test]
    fn test_decide_routing_reject_in_per_command_mode() {
        // Per-command mode should reject stateful commands
        assert_eq!(
            decide_command_routing("GROUP alt.test", true, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_reject_in_stateful_mode() {
        // Stateful mode should reject non-routable commands
        assert_eq!(
            decide_command_routing("POST", true, false, RoutingMode::Stateful),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_capabilities_always_intercepted() {
        // RFC 4643 §3.1: CAPABILITIES must be accessible before authentication,
        // so it must always be intercepted regardless of auth state or routing mode.
        assert_eq!(
            decide_command_routing("CAPABILITIES", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::InterceptCapabilities,
            "CAPABILITIES should be intercepted even when auth required"
        );
        assert_eq!(
            decide_command_routing("CAPABILITIES", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::InterceptCapabilities,
            "CAPABILITIES should be intercepted when authenticated"
        );
        assert_eq!(
            decide_command_routing("CAPABILITIES", false, false, RoutingMode::Hybrid),
            CommandRoutingDecision::InterceptCapabilities,
            "CAPABILITIES should be intercepted in hybrid mode"
        );
        assert_eq!(
            decide_command_routing("CAPABILITIES", true, false, RoutingMode::Stateful),
            CommandRoutingDecision::InterceptCapabilities,
            "CAPABILITIES should be intercepted in stateful mode"
        );
    }

    #[test]
    fn test_decide_routing_hybrid_mode_stateless_forwarded() {
        // Hybrid mode with stateless command should forward
        assert_eq!(
            decide_command_routing("LIST", true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_hybrid_mode_reject_non_stateful() {
        // Hybrid mode with rejected but non-stateful command (like POST) should just reject
        assert_eq!(
            decide_command_routing("POST", true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_all_modes_with_stateful_commands() {
        let stateful_commands = vec!["GROUP alt.test", "NEXT", "LAST", "XOVER 1-100"];

        for cmd in stateful_commands {
            // Hybrid mode: switch to stateful
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::Hybrid),
                CommandRoutingDecision::SwitchToStateful,
                "Failed for command: {cmd}"
            );

            // Per-command mode: reject
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::PerCommand),
                CommandRoutingDecision::Reject,
                "Failed for command: {cmd}"
            );

            // Stateful mode: reject (though shouldn't reach this in practice)
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::Stateful),
                CommandRoutingDecision::Reject,
                "Failed for command: {cmd}"
            );
        }
    }

    #[test]
    fn test_decide_routing_auth_flow_progression() {
        // Step 1: Not authenticated, auth enabled -> require auth
        assert_eq!(
            decide_command_routing("LIST", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RequireAuth
        );

        // Step 2: Authenticated, auth enabled -> forward
        assert_eq!(
            decide_command_routing("LIST", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_request_routing_unknown_extensions_are_stateful() {
        let request = RequestContext::from_request_bytes(b"XFOO arg\r\n");
        assert_eq!(
            decide_request_routing(&request, true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::SwitchToStateful
        );
        assert_eq!(
            decide_request_routing(&request, true, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Reject
        );
    }
}
