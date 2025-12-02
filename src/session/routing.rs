//! Routing decision logic for session management
//!
//! Encapsulates the complex decision tree for determining how commands should be routed
//! based on session mode, routing configuration, and router availability.

use crate::config::RoutingMode;
use crate::router::BackendSelector;
use std::sync::Arc;

use super::SessionMode;

/// Routing decision encapsulates the mode logic for a session
///
/// This type aggregates the routing-related state and provides clean decision methods,
/// eliminating the need for complex boolean expressions scattered throughout the code.
///
/// # Examples
///
/// ```
/// use nntp_proxy::session::routing::RoutingDecision;
/// use nntp_proxy::session::SessionMode;
/// use nntp_proxy::config::RoutingMode;
///
/// // Per-command routing
/// let decision = RoutingDecision::new(
///     SessionMode::PerCommand,
///     RoutingMode::PerCommand,
///     true,
/// );
/// assert!(decision.should_use_per_command_routing());
/// assert!(!decision.can_switch_to_stateful());
///
/// // Hybrid mode (can switch)
/// let decision = RoutingDecision::new(
///     SessionMode::PerCommand,
///     RoutingMode::Hybrid,
///     true,
/// );
/// assert!(decision.can_switch_to_stateful());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingDecision {
    /// Current session mode
    mode: SessionMode,
    /// Configured routing mode
    routing_mode: RoutingMode,
    /// Whether a router is available
    has_router: bool,
}

impl RoutingDecision {
    /// Create a new routing decision
    #[must_use]
    pub const fn new(mode: SessionMode, routing_mode: RoutingMode, has_router: bool) -> Self {
        Self {
            mode,
            routing_mode,
            has_router,
        }
    }

    /// Create routing decision from session state
    #[must_use]
    pub fn from_session(
        mode: SessionMode,
        routing_mode: RoutingMode,
        router: &Option<Arc<BackendSelector>>,
    ) -> Self {
        Self::new(mode, routing_mode, router.is_some())
    }

    /// Should this session use per-command routing?
    ///
    /// Returns true if:
    /// - A router is available
    /// - Current mode is PerCommand
    #[must_use]
    #[inline]
    pub const fn should_use_per_command_routing(&self) -> bool {
        self.has_router && matches!(self.mode, SessionMode::PerCommand)
    }

    /// Can this session switch to stateful mode?
    ///
    /// Returns true if:
    /// - Routing mode is Hybrid
    /// - Not already in Stateful mode
    #[must_use]
    #[inline]
    pub const fn can_switch_to_stateful(&self) -> bool {
        matches!(self.routing_mode, RoutingMode::Hybrid)
            && matches!(self.mode, SessionMode::PerCommand)
    }

    /// Is this session in stateful mode?
    #[must_use]
    #[inline]
    pub const fn is_stateful(&self) -> bool {
        matches!(self.mode, SessionMode::Stateful)
    }

    /// Is this session in per-command mode?
    #[must_use]
    #[inline]
    pub const fn is_per_command(&self) -> bool {
        matches!(self.mode, SessionMode::PerCommand)
    }

    /// Get the current session mode
    #[must_use]
    #[inline]
    pub const fn mode(&self) -> SessionMode {
        self.mode
    }

    /// Get the configured routing mode
    #[must_use]
    #[inline]
    pub const fn routing_mode(&self) -> RoutingMode {
        self.routing_mode
    }

    /// Does this session have a router available?
    #[must_use]
    #[inline]
    pub const fn has_router(&self) -> bool {
        self.has_router
    }

    /// Transition to stateful mode
    ///
    /// Returns a new RoutingDecision with mode = Stateful
    #[must_use]
    pub const fn transition_to_stateful(self) -> Self {
        Self {
            mode: SessionMode::Stateful,
            routing_mode: self.routing_mode,
            has_router: self.has_router,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_per_command_routing_requires_both() {
        // Has router but in Stateful mode
        let decision = RoutingDecision::new(SessionMode::Stateful, RoutingMode::PerCommand, true);
        assert!(!decision.should_use_per_command_routing());

        // In PerCommand mode but no router
        let decision =
            RoutingDecision::new(SessionMode::PerCommand, RoutingMode::PerCommand, false);
        assert!(!decision.should_use_per_command_routing());

        // Both conditions met
        let decision = RoutingDecision::new(SessionMode::PerCommand, RoutingMode::PerCommand, true);
        assert!(decision.should_use_per_command_routing());
    }

    #[test]
    fn test_can_switch_to_stateful() {
        // Hybrid + PerCommand = can switch
        let decision = RoutingDecision::new(SessionMode::PerCommand, RoutingMode::Hybrid, true);
        assert!(decision.can_switch_to_stateful());

        // Hybrid + Stateful = can't switch (already there)
        let decision = RoutingDecision::new(SessionMode::Stateful, RoutingMode::Hybrid, true);
        assert!(!decision.can_switch_to_stateful());

        // PerCommand mode (not Hybrid) = can't switch
        let decision = RoutingDecision::new(SessionMode::PerCommand, RoutingMode::PerCommand, true);
        assert!(!decision.can_switch_to_stateful());

        // Stateful mode = can't switch
        let decision = RoutingDecision::new(SessionMode::Stateful, RoutingMode::Stateful, true);
        assert!(!decision.can_switch_to_stateful());
    }

    #[test]
    fn test_mode_queries() {
        let decision = RoutingDecision::new(SessionMode::PerCommand, RoutingMode::Hybrid, true);
        assert!(decision.is_per_command());
        assert!(!decision.is_stateful());
        assert_eq!(decision.mode(), SessionMode::PerCommand);

        let decision = RoutingDecision::new(SessionMode::Stateful, RoutingMode::Stateful, false);
        assert!(decision.is_stateful());
        assert!(!decision.is_per_command());
        assert_eq!(decision.mode(), SessionMode::Stateful);
    }

    #[test]
    fn test_transition_to_stateful() {
        let decision = RoutingDecision::new(SessionMode::PerCommand, RoutingMode::Hybrid, true);
        let transitioned = decision.transition_to_stateful();

        assert_eq!(transitioned.mode(), SessionMode::Stateful);
        assert_eq!(transitioned.routing_mode(), RoutingMode::Hybrid);
        assert!(transitioned.has_router());
        assert!(!transitioned.can_switch_to_stateful());
    }

    #[test]
    fn test_accessors() {
        let decision = RoutingDecision::new(SessionMode::PerCommand, RoutingMode::Hybrid, true);

        assert_eq!(decision.mode(), SessionMode::PerCommand);
        assert_eq!(decision.routing_mode(), RoutingMode::Hybrid);
        assert!(decision.has_router());
    }

    #[test]
    fn test_const_functions() {
        // All functions should be const
        const DECISION: RoutingDecision =
            RoutingDecision::new(SessionMode::PerCommand, RoutingMode::Hybrid, true);
        const HAS_ROUTER: bool = DECISION.has_router();
        const MODE: SessionMode = DECISION.mode();
        const ROUTING_MODE: RoutingMode = DECISION.routing_mode();

        assert!(HAS_ROUTER);
        assert!(matches!(MODE, SessionMode::PerCommand));
        assert!(matches!(ROUTING_MODE, RoutingMode::Hybrid));
    }
}
