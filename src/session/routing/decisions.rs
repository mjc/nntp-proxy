//! Command routing decisions
//!
//! Pure functions for determining how to route NNTP commands based on
//! authentication state and routing mode.

use crate::command::{CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;

/// Decision for how to handle a command in per-command routing mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandRoutingDecision {
    /// Intercept and handle authentication locally
    InterceptAuth,
    /// Forward command to backend (authenticated or auth disabled)
    Forward,
    /// Require authentication first
    RequireAuth,
    /// Switch to stateful mode (hybrid mode only)
    SwitchToStateful,
    /// Reject the command
    Reject,
    /// Reject AUTHINFO after successful authentication
    RejectAuthSequence,
    /// Reject MODE READER after successful authentication
    RejectModeReaderAfterAuth,
}

/// Determine how to handle a command based on auth state and routing mode
///
/// This is a pure function that can be easily tested without I/O dependencies.
///
/// # Arguments
/// * `command` - The raw NNTP command string
/// * `is_authenticated` - Whether the client has authenticated
/// * `auth_enabled` - Whether authentication is required
/// * `routing_mode` - Current routing mode (PerCommand, Stateful, Hybrid)
///
/// # Returns
/// A `CommandRoutingDecision` indicating what action to take
pub(crate) fn decide_command_routing(
    command: &str,
    is_authenticated: bool,
    auth_enabled: bool,
    routing_mode: RoutingMode,
) -> CommandRoutingDecision {
    use CommandAction::*;

    // Classify the command
    let action = CommandHandler::classify(command);
    let is_authinfo = crate::session::common::is_authinfo_command(command);
    let is_capabilities = command
        .split_ascii_whitespace()
        .next()
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case("CAPABILITIES"));
    let is_mode_reader = crate::session::common::is_mode_reader_command(command);

    if is_authinfo {
        if !auth_enabled {
            return CommandRoutingDecision::Forward;
        }
        if is_authenticated {
            return CommandRoutingDecision::RejectAuthSequence;
        }
    }

    match action {
        InterceptAuth(_) => CommandRoutingDecision::InterceptAuth,

        // Stateless commands
        ForwardStateless => {
            if is_authenticated && auth_enabled && is_mode_reader {
                CommandRoutingDecision::RejectModeReaderAfterAuth
            } else if is_authenticated || !auth_enabled || is_capabilities {
                CommandRoutingDecision::Forward
            } else {
                CommandRoutingDecision::RequireAuth
            }
        }

        // Rejected commands in hybrid mode with stateful command -> switch mode
        Reject(_)
            if routing_mode == RoutingMode::Hybrid && NntpCommand::parse(command).is_stateful() =>
        {
            CommandRoutingDecision::SwitchToStateful
        }

        // All other rejected commands
        Reject(_) => CommandRoutingDecision::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decide_routing_auth_commands_always_intercepted() {
        // Auth commands should always be intercepted regardless of other flags
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RejectAuthSequence
        );
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", false, false, RoutingMode::Stateful),
            CommandRoutingDecision::Forward
        );
        assert_eq!(
            decide_command_routing(
                "AUTHINFO SASL EXAMPLE",
                false,
                false,
                RoutingMode::PerCommand
            ),
            CommandRoutingDecision::Forward
        );
        assert_eq!(
            decide_command_routing("AUTHINFO SASL EXAMPLE", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RejectAuthSequence
        );
        assert_eq!(
            decide_command_routing(
                "AUTHINFO SASL EXAMPLE",
                false,
                true,
                RoutingMode::PerCommand
            ),
            CommandRoutingDecision::Reject
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
    fn test_decide_routing_capabilities_allowed_before_auth() {
        assert_eq!(
            decide_command_routing("CAPABILITIES", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
        assert_eq!(
            decide_command_routing("cApAbIlItIeS", false, true, RoutingMode::Stateful),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_mode_reader_rejected_after_authentication() {
        assert_eq!(
            decide_command_routing("MODE READER", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RejectModeReaderAfterAuth
        );
        assert_eq!(
            decide_command_routing("mode reader", true, true, RoutingMode::Stateful),
            CommandRoutingDecision::RejectModeReaderAfterAuth
        );
        assert_eq!(
            decide_command_routing("MODE READER", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RequireAuth
        );
        assert_eq!(
            decide_command_routing("MODE READER", true, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
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
                "Failed for command: {}",
                cmd
            );

            // Per-command mode: reject
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::PerCommand),
                CommandRoutingDecision::Reject,
                "Failed for command: {}",
                cmd
            );

            // Stateful mode: reject (though shouldn't reach this in practice)
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::Stateful),
                CommandRoutingDecision::Reject,
                "Failed for command: {}",
                cmd
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
}
