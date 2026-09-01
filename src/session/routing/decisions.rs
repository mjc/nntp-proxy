//! Command routing decisions
//!
//! Pure functions for determining how to route NNTP commands based on
//! authentication state and routing mode.

#[cfg(test)]
mod tests {
    use crate::command::{AuthAction, CommandHandler, CommandPlan};
    use crate::config::RoutingMode;
    use crate::protocol::RequestContext;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CommandRoutingDecision {
        InterceptAuth,
        InterceptCapabilities,
        Forward,
        RequireAuth,
        SwitchToStateful,
        Reject,
    }

    fn decide_command_routing(
        command: &str,
        is_authenticated: bool,
        auth_enabled: bool,
        routing_mode: RoutingMode,
    ) -> CommandRoutingDecision {
        let request = RequestContext::parse(command.as_bytes()).expect("valid request line");
        classify_request_kind(&request, is_authenticated, auth_enabled, routing_mode)
    }

    fn classify_request_kind(
        request: &RequestContext,
        is_authenticated: bool,
        auth_enabled: bool,
        routing_mode: RoutingMode,
    ) -> CommandRoutingDecision {
        plan_kind(CommandHandler::classify_request(
            request,
            is_authenticated,
            auth_enabled,
            routing_mode,
        ))
    }

    fn plan_kind(plan: CommandPlan<'_>) -> CommandRoutingDecision {
        match plan {
            CommandPlan::InterceptAuth(_) => CommandRoutingDecision::InterceptAuth,
            CommandPlan::InterceptCapabilities => CommandRoutingDecision::InterceptCapabilities,
            CommandPlan::Forward => CommandRoutingDecision::Forward,
            CommandPlan::RequireAuth => CommandRoutingDecision::RequireAuth,
            CommandPlan::SwitchToStateful => CommandRoutingDecision::SwitchToStateful,
            CommandPlan::Reject(_) => CommandRoutingDecision::Reject,
        }
    }

    #[test]
    fn canonical_plan_carries_auth_and_rejection_payloads() {
        let auth_request = RequestContext::parse(b"AUTHINFO USER alice\r\n").unwrap();
        assert!(matches!(
            CommandHandler::classify_request(&auth_request, false, true, RoutingMode::PerCommand,),
            CommandPlan::InterceptAuth(AuthAction::RequestPassword("alice"))
        ));

        let post_request = RequestContext::parse(b"POST\r\n").unwrap();
        assert!(matches!(
            CommandHandler::classify_request(
                &post_request,
                true,
                true,
                RoutingMode::PerCommand,
            ),
            CommandPlan::Reject(response) if response.status().as_u16() == 440
        ));
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
    fn test_decide_routing_hybrid_stateful_requires_auth_when_enabled() {
        assert_eq!(
            decide_command_routing("GROUP alt.test", false, true, RoutingMode::Hybrid),
            CommandRoutingDecision::RequireAuth
        );
        assert_eq!(
            decide_command_routing("XOVER 1-100", false, true, RoutingMode::Hybrid),
            CommandRoutingDecision::RequireAuth
        );
        assert_eq!(
            decide_command_routing("GROUP alt.test", true, true, RoutingMode::Hybrid),
            CommandRoutingDecision::SwitchToStateful
        );
    }

    #[test]
    fn test_decide_routing_hybrid_unknown_extensions_require_auth_when_enabled() {
        let request = RequestContext::parse(b"XFOO arg\r\n").expect("valid request line");
        assert_eq!(
            classify_request_kind(&request, false, true, RoutingMode::Hybrid),
            CommandRoutingDecision::RequireAuth
        );
        assert_eq!(
            classify_request_kind(&request, true, true, RoutingMode::Hybrid),
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
    fn test_decide_routing_requires_auth_before_rejecting_unsupported_commands() {
        for mode in [
            RoutingMode::PerCommand,
            RoutingMode::Hybrid,
            RoutingMode::Stateful,
        ] {
            assert_eq!(
                decide_command_routing("POST", false, true, mode),
                CommandRoutingDecision::RequireAuth,
                "POST should be auth-gated before command policy is revealed in {mode:?}"
            );
            assert_eq!(
                decide_command_routing("STARTTLS", false, true, mode),
                CommandRoutingDecision::RequireAuth,
                "STARTTLS should be auth-gated before command policy is revealed in {mode:?}"
            );
        }

        assert_eq!(
            decide_command_routing("POST", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::Reject
        );
        assert_eq!(
            decide_command_routing("STARTTLS", true, true, RoutingMode::Stateful),
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
        let request = RequestContext::parse(b"XFOO arg\r\n").expect("valid request line");
        assert_eq!(
            classify_request_kind(&request, true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::SwitchToStateful
        );
        assert_eq!(
            classify_request_kind(&request, false, true, RoutingMode::Hybrid),
            CommandRoutingDecision::RequireAuth
        );
        assert_eq!(
            classify_request_kind(&request, true, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Reject
        );
    }
}
