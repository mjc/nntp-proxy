//! Session mode state management
//!
//! This module provides a type-safe wrapper for session mode and routing mode,
//! ensuring valid state transitions and clear mode switching logic.

use crate::config::RoutingMode;
use std::sync::atomic::{AtomicU8, Ordering};

/// Session mode - determines how commands are routed
///
/// This is separate from `RoutingMode` (configuration) - it represents the
/// *current* runtime state of the session, which can change (e.g., hybrid mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionMode {
    /// Per-command routing - each command may go to different backend
    PerCommand = 0,

    /// Stateful mode - using a dedicated backend connection
    Stateful = 1,
}

impl SessionMode {
    /// Check if this mode is per-command routing
    #[inline]
    #[must_use]
    pub const fn is_per_command(self) -> bool {
        matches!(self, Self::PerCommand)
    }

    /// Check if this mode is stateful
    #[inline]
    #[must_use]
    pub const fn is_stateful(self) -> bool {
        matches!(self, Self::Stateful)
    }

    /// Convert to u8 for atomic storage
    #[inline]
    const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Convert from u8 from atomic storage
    #[inline]
    const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::PerCommand,
            1 => Self::Stateful,
            _ => Self::PerCommand, // Safe default
        }
    }
}

/// Manages session mode state with support for runtime transitions
///
/// This type encapsulates the current session mode and routing mode configuration,
/// providing thread-safe mode transitions for hybrid routing.
///
/// # Design
///
/// - **Current Mode**: `AtomicU8` for lock-free concurrent reads/writes
/// - **Routing Mode**: Immutable configuration (Stateful, PerCommand, or Hybrid)
/// - Mode transitions are only allowed in Hybrid mode
///
/// # One-Way Transition Invariant
///
/// **CRITICAL**: In Hybrid mode, the transition from PerCommand → Stateful is
/// **permanent and irreversible** for the lifetime of the connection:
///
/// ```text
/// PerCommand ──stateful command──> Stateful
///     ↑                               │
///     └───────── NO WAY BACK ─────────┘
/// ```
///
/// Once `switch_to_stateful()` is called:
/// - Connection acquires a dedicated backend
/// - All subsequent commands use that backend
/// - Connection stays stateful until client disconnects
/// - New client connection starts fresh in PerCommand mode (if Hybrid)
///
/// # Examples
///
/// ```
/// use nntp_proxy::session::{ModeState, SessionMode};
/// use nntp_proxy::config::RoutingMode;
///
/// // Stateful mode (no transitions allowed)
/// let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
/// assert!(state.is_stateful());
/// assert!(!state.can_switch_mode());
///
/// // Hybrid mode (starts per-command, can switch)
/// let state = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
/// assert!(state.is_per_command());
/// assert!(state.can_switch_mode());
///
/// state.switch_to_stateful();
/// assert!(state.is_stateful());
/// // Now permanently stateful for this connection
/// ```
#[derive(Debug)]
pub struct ModeState {
    /// Current session mode (can change at runtime in Hybrid mode)
    mode: AtomicU8,

    /// Routing mode configuration (immutable)
    routing_mode: RoutingMode,
}

impl ModeState {
    /// Create a new mode state
    ///
    /// # Arguments
    ///
    /// * `initial_mode` - Initial session mode
    /// * `routing_mode` - Routing mode configuration
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
    /// assert!(state.is_stateful());
    /// ```
    #[inline]
    #[must_use]
    pub fn new(initial_mode: SessionMode, routing_mode: RoutingMode) -> Self {
        Self {
            mode: AtomicU8::new(initial_mode.to_u8()),
            routing_mode,
        }
    }

    /// Get the current session mode
    ///
    /// This is a cheap atomic load operation.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let state = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
    /// assert_eq!(state.mode(), SessionMode::PerCommand);
    /// ```
    #[inline]
    #[must_use]
    pub fn mode(&self) -> SessionMode {
        SessionMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Get the routing mode configuration
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
    /// assert_eq!(state.routing_mode(), RoutingMode::Stateful);
    /// ```
    #[inline]
    #[must_use]
    pub const fn routing_mode(&self) -> RoutingMode {
        self.routing_mode
    }

    /// Check if currently in per-command mode
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let state = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
    /// assert!(state.is_per_command());
    /// ```
    #[inline]
    #[must_use]
    pub fn is_per_command(&self) -> bool {
        self.mode().is_per_command()
    }

    /// Check if currently in stateful mode
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
    /// assert!(state.is_stateful());
    /// ```
    #[inline]
    #[must_use]
    pub fn is_stateful(&self) -> bool {
        self.mode().is_stateful()
    }

    /// Check if mode switching is allowed
    ///
    /// Mode switching is only allowed in Hybrid routing mode.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let hybrid = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
    /// assert!(hybrid.can_switch_mode());
    ///
    /// let stateful = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
    /// assert!(!stateful.can_switch_mode());
    /// ```
    #[inline]
    #[must_use]
    pub const fn can_switch_mode(&self) -> bool {
        matches!(self.routing_mode, RoutingMode::Hybrid)
    }

    /// Switch to stateful mode (one-way transition)
    ///
    /// **IMPORTANT**: This is a **permanent, one-way transition** for this connection.
    /// Once switched from per-command to stateful mode, the connection remains
    /// stateful for its entire lifetime and **never switches back**.
    ///
    /// This transition happens in Hybrid mode when:
    /// - Client issues a stateful command (GROUP, NEXT, LAST, XOVER, etc.)
    /// - Client needs server-side state maintained across commands
    /// - Connection acquires a dedicated backend and keeps it until disconnect
    ///
    /// Only allowed in Hybrid routing mode. No-op if already stateful or
    /// if routing mode doesn't allow switching.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let state = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
    /// assert!(state.is_per_command());
    ///
    /// // Client sends "GROUP alt.binaries.test"
    /// state.switch_to_stateful();
    /// assert!(state.is_stateful());
    ///
    /// // Connection stays stateful until client disconnects
    /// // (no way to switch back to per-command)
    /// ```
    #[inline]
    pub fn switch_to_stateful(&self) {
        if self.can_switch_mode() {
            self.mode
                .store(SessionMode::Stateful.to_u8(), Ordering::Relaxed);
        }
    }

    /// Check if this session is using per-command routing
    ///
    /// Returns true if routing_mode is PerCommand or Hybrid.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{ModeState, SessionMode};
    /// use nntp_proxy::config::RoutingMode;
    ///
    /// let per_cmd = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
    /// assert!(per_cmd.is_per_command_routing());
    ///
    /// let hybrid = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
    /// assert!(hybrid.is_per_command_routing());
    ///
    /// let stateful = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
    /// assert!(!stateful.is_per_command_routing());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_per_command_routing(&self) -> bool {
        matches!(
            self.routing_mode,
            RoutingMode::PerCommand | RoutingMode::Hybrid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_mode_is_per_command() {
        assert!(SessionMode::PerCommand.is_per_command());
        assert!(!SessionMode::Stateful.is_per_command());
    }

    #[test]
    fn test_session_mode_is_stateful() {
        assert!(SessionMode::Stateful.is_stateful());
        assert!(!SessionMode::PerCommand.is_stateful());
    }

    #[test]
    fn test_session_mode_roundtrip() {
        assert_eq!(
            SessionMode::from_u8(SessionMode::PerCommand.to_u8()),
            SessionMode::PerCommand
        );
        assert_eq!(
            SessionMode::from_u8(SessionMode::Stateful.to_u8()),
            SessionMode::Stateful
        );
    }

    #[test]
    fn test_mode_state_new() {
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
        assert_eq!(state.mode(), SessionMode::PerCommand);
        assert_eq!(state.routing_mode(), RoutingMode::PerCommand);
    }

    #[test]
    fn test_mode_state_is_per_command() {
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
        assert!(state.is_per_command());
        assert!(!state.is_stateful());
    }

    #[test]
    fn test_mode_state_is_stateful() {
        let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
        assert!(state.is_stateful());
        assert!(!state.is_per_command());
    }

    #[test]
    fn test_can_switch_mode_hybrid() {
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
        assert!(state.can_switch_mode());
    }

    #[test]
    fn test_cannot_switch_mode_stateful() {
        let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
        assert!(!state.can_switch_mode());
    }

    #[test]
    fn test_cannot_switch_mode_per_command() {
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
        assert!(!state.can_switch_mode());
    }

    #[test]
    fn test_switch_to_stateful_in_hybrid() {
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
        assert!(state.is_per_command());

        state.switch_to_stateful();
        assert!(state.is_stateful());
    }

    #[test]
    fn test_switch_to_stateful_noop_in_stateful_mode() {
        let state = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
        assert!(state.is_stateful());

        state.switch_to_stateful();
        assert!(state.is_stateful());
    }

    #[test]
    fn test_switch_to_stateful_noop_in_per_command_mode() {
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
        assert!(state.is_per_command());

        state.switch_to_stateful();
        // Should remain in per-command mode
        assert!(state.is_per_command());
    }

    #[test]
    fn test_is_per_command_routing() {
        let per_cmd = ModeState::new(SessionMode::PerCommand, RoutingMode::PerCommand);
        assert!(per_cmd.is_per_command_routing());

        let hybrid = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
        assert!(hybrid.is_per_command_routing());

        let stateful = ModeState::new(SessionMode::Stateful, RoutingMode::Stateful);
        assert!(!stateful.is_per_command_routing());
    }

    #[test]
    fn test_one_way_transition_invariant() {
        // Once switched to stateful in hybrid mode, stays stateful forever
        let state = ModeState::new(SessionMode::PerCommand, RoutingMode::Hybrid);
        assert!(state.is_per_command());

        // Simulate client sending "GROUP alt.test"
        state.switch_to_stateful();
        assert!(state.is_stateful());

        // No way to switch back - would need new connection
        // (No switch_to_per_command() method exists)

        // Verify it stays stateful
        assert!(state.is_stateful());
        assert!(!state.is_per_command());

        // Can call switch_to_stateful again (no-op)
        state.switch_to_stateful();
        assert!(state.is_stateful());
    }
}
